use std::{fs, path::PathBuf, sync::Arc};

use {
    matrix_sdk::{
        Client, Room,
        config::SyncSettings,
        encryption::{BackupDownloadStrategy, EncryptionSettings, recovery::RecoveryState},
        ruma::{
            OwnedUserId,
            api::client::uiaa::{AuthData, Password, UserIdentifier},
            events::room::encrypted::OriginalSyncRoomEncryptedEvent,
        },
    },
    reqwest::StatusCode,
    secrecy::ExposeSecret,
    serde::Deserialize,
    tokio_util::sync::CancellationToken,
    tracing::{info, instrument, warn},
};

use moltis_channels::{Error as ChannelError, Result as ChannelResult};

use crate::{
    config::{MatrixAccountConfig, MatrixOwnershipMode},
    handler,
    state::AccountStateMap,
    verification,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthMode {
    AccessToken,
    Password,
}

#[derive(Debug, Clone)]
pub(crate) struct AuthenticatedMatrixAccount {
    pub user_id: OwnedUserId,
    pub ownership_startup_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AccessTokenIdentity {
    user_id: OwnedUserId,
    device_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AccessTokenWhoAmIResponse {
    user_id: OwnedUserId,
    #[serde(default)]
    device_id: Option<String>,
}

#[instrument(skip(config), fields(account_id, homeserver = %config.homeserver))]
pub(crate) async fn build_client(
    account_id: &str,
    config: &MatrixAccountConfig,
) -> ChannelResult<Client> {
    let store_path = ensure_store_path(account_id)?;
    Client::builder()
        .homeserver_url(&config.homeserver)
        .with_encryption_settings(encryption_settings())
        .sqlite_store(&store_path, None)
        .build()
        .await
        .map_err(|error| ChannelError::external("matrix client build", error))
}

fn encryption_settings() -> EncryptionSettings {
    EncryptionSettings {
        auto_enable_cross_signing: true,
        backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
        ..Default::default()
    }
}

pub(crate) fn auth_mode(config: &MatrixAccountConfig) -> ChannelResult<AuthMode> {
    let access_token = config.access_token.expose_secret().trim();
    if !access_token.is_empty() && access_token != moltis_common::secret_serde::REDACTED {
        return Ok(AuthMode::AccessToken);
    }

    let password = config
        .password
        .as_ref()
        .map(|secret| secret.expose_secret().trim())
        .unwrap_or_default();
    if password.is_empty() || password == moltis_common::secret_serde::REDACTED {
        return Err(ChannelError::invalid_input(
            "either access_token or password is required",
        ));
    }

    if config.user_id.as_deref().is_none_or(str::is_empty) {
        return Err(ChannelError::invalid_input(
            "user_id is required when using password authentication",
        ));
    }

    Ok(AuthMode::Password)
}

#[instrument(skip(client, config), fields(account_id))]
pub(crate) async fn authenticate_client(
    client: &Client,
    account_id: &str,
    config: &MatrixAccountConfig,
) -> ChannelResult<AuthenticatedMatrixAccount> {
    match auth_mode(config)? {
        AuthMode::AccessToken => {
            let identity = restore_access_token_session(client, account_id, config).await?;
            client
                .encryption()
                .wait_for_e2ee_initialization_tasks()
                .await;
            info!(
                account_id,
                user_id = %identity.user_id,
                device_id = identity.device_id.as_deref().unwrap_or("<unknown>"),
                "matrix session restored"
            );
            Ok(AuthenticatedMatrixAccount {
                user_id: identity.user_id,
                ownership_startup_error: None,
            })
        },
        AuthMode::Password => {
            login_with_password(client, account_id, config).await?;
            client
                .encryption()
                .wait_for_e2ee_initialization_tasks()
                .await;
            let bot_user_id = client
                .whoami()
                .await
                .map_err(|error| ChannelError::external("matrix whoami", error))?
                .user_id;
            let ownership_startup_error =
                maybe_take_matrix_account_ownership(client, account_id, config).await;
            info!(account_id, user_id = %bot_user_id, "matrix password login complete");
            Ok(AuthenticatedMatrixAccount {
                user_id: bot_user_id,
                ownership_startup_error,
            })
        },
    }
}

async fn maybe_take_matrix_account_ownership(
    client: &Client,
    account_id: &str,
    config: &MatrixAccountConfig,
) -> Option<String> {
    if config.ownership_mode != MatrixOwnershipMode::MoltisOwned {
        return None;
    }

    match ensure_moltis_owned_encryption_state(client, account_id, config).await {
        Ok(()) => None,
        Err(error) => {
            warn!(account_id, error = %error, "matrix ownership setup failed");
            Some(error.to_string())
        },
    }
}

#[instrument(skip(client, config), fields(account_id))]
async fn ensure_moltis_owned_encryption_state(
    client: &Client,
    account_id: &str,
    config: &MatrixAccountConfig,
) -> ChannelResult<()> {
    let Some(user_id) = config
        .user_id
        .as_deref()
        .filter(|user_id| !user_id.is_empty())
    else {
        return Err(ChannelError::invalid_input(
            "user_id is required when Moltis owns a Matrix account",
        ));
    };
    let Some(password) = config.password.as_ref() else {
        return Err(ChannelError::invalid_input(
            "password is required when Moltis owns a Matrix account",
        ));
    };

    bootstrap_cross_signing_with_password(client, user_id, password.expose_secret()).await?;

    if !cross_signing_is_complete(client).await {
        force_take_over_existing_identity(client, account_id, user_id, password.expose_secret())
            .await?;
    }

    match client.encryption().recovery().state() {
        RecoveryState::Disabled => {
            enable_password_backed_recovery(client, password.expose_secret()).await?;
            info!(
                account_id,
                "matrix ownership recovery enabled with password-backed secret storage"
            );
        },
        RecoveryState::Enabled => {
            info!(account_id, "matrix ownership recovery already enabled");
        },
        RecoveryState::Incomplete => {
            match client
                .encryption()
                .recovery()
                .recover(password.expose_secret())
                .await
            {
                Ok(()) => {
                    info!(
                        account_id,
                        "matrix ownership recovered existing secret storage with account password"
                    );
                },
                Err(error) => {
                    warn!(
                        account_id,
                        error = %error,
                        "matrix ownership could not recover existing secret storage with account password"
                    );
                    force_take_over_existing_identity(
                        client,
                        account_id,
                        user_id,
                        password.expose_secret(),
                    )
                    .await?;
                },
            }
        },
        RecoveryState::Unknown => {
            warn!(
                account_id,
                "matrix recovery state is still unknown after login, skipping automatic ownership bootstrap"
            );
        },
    }

    ensure_own_device_is_cross_signed(client).await?;

    if !cross_signing_is_complete(client).await {
        return Err(ChannelError::invalid_input(
            "matrix ownership bootstrap completed but cross-signing is still incomplete",
        ));
    }

    Ok(())
}

async fn enable_password_backed_recovery(client: &Client, password: &str) -> ChannelResult<String> {
    client
        .encryption()
        .recovery()
        .enable()
        .wait_for_backups_to_upload()
        .with_passphrase(password)
        .await
        .map_err(|error| ChannelError::external("matrix recovery enable", error))
}

async fn ensure_own_device_is_cross_signed(client: &Client) -> ChannelResult<()> {
    let Some(own_device) = client
        .encryption()
        .get_own_device()
        .await
        .map_err(|error| ChannelError::external("matrix own device lookup", error))?
    else {
        return Ok(());
    };

    if own_device.is_cross_signed_by_owner() {
        return Ok(());
    }

    own_device
        .verify()
        .await
        .map_err(|error| ChannelError::external("matrix own device self-sign", error))
}

async fn cross_signing_is_complete(client: &Client) -> bool {
    client
        .encryption()
        .cross_signing_status()
        .await
        .is_some_and(|status| status.is_complete())
}

#[instrument(skip(client, password), fields(account_id))]
async fn force_take_over_existing_identity(
    client: &Client,
    account_id: &str,
    user_id: &str,
    password: &str,
) -> ChannelResult<()> {
    let maybe_handle = client
        .encryption()
        .recovery()
        .reset_identity()
        .await
        .map_err(|error| ChannelError::external("matrix recovery reset identity", error))?;

    if let Some(handle) = maybe_handle {
        match handle.auth_type() {
            matrix_sdk::encryption::CrossSigningResetAuthType::Uiaa(uiaa) => {
                let mut auth = Password::new(
                    UserIdentifier::UserIdOrLocalpart(user_id.to_owned()),
                    password.to_owned(),
                );
                auth.session = uiaa.session.clone();
                handle
                    .reset(Some(AuthData::Password(auth)))
                    .await
                    .map_err(|error| {
                        ChannelError::external("matrix recovery reset identity auth", error)
                    })?;
            },
            matrix_sdk::encryption::CrossSigningResetAuthType::OAuth(info) => {
                return Err(ChannelError::invalid_input(format!(
                    "matrix account requires browser approval to reset cross-signing at {}; complete that in Element or switch to user-managed mode",
                    info.approval_url
                )));
            },
        }
    }

    let _recovery_key = enable_password_backed_recovery(client, password).await?;

    info!(
        account_id,
        "matrix ownership forcibly reset existing recovery state and bootstrapped fresh Moltis-managed recovery"
    );

    Ok(())
}

async fn bootstrap_cross_signing_with_password(
    client: &Client,
    user_id: &str,
    password: &str,
) -> ChannelResult<()> {
    match client
        .encryption()
        .bootstrap_cross_signing_if_needed(None)
        .await
    {
        Ok(()) => Ok(()),
        Err(error) => {
            let Some(response) = error.as_uiaa_response() else {
                return Err(ChannelError::external(
                    "matrix cross-signing bootstrap",
                    error,
                ));
            };

            let mut auth = Password::new(
                UserIdentifier::UserIdOrLocalpart(user_id.to_owned()),
                password.to_owned(),
            );
            auth.session = response.session.clone();

            client
                .encryption()
                .bootstrap_cross_signing(Some(AuthData::Password(auth)))
                .await
                .map_err(|error| ChannelError::external("matrix cross-signing bootstrap", error))
        },
    }
}

#[instrument(skip(client, accounts), fields(account_id, user_id = %bot_user_id))]
pub(crate) fn register_event_handlers(
    client: &Client,
    account_id: &str,
    accounts: &AccountStateMap,
    bot_user_id: &OwnedUserId,
) {
    let accounts_for_msg = Arc::clone(accounts);
    let account_id_msg = account_id.to_string();
    let bot_uid_msg = bot_user_id.clone();
    client.add_event_handler(
        move |ev: matrix_sdk::ruma::events::room::message::OriginalSyncRoomMessageEvent,
              room: Room| {
            let accounts = Arc::clone(&accounts_for_msg);
            let aid = account_id_msg.clone();
            let buid = bot_uid_msg.clone();
            async move {
                handler::handle_room_message(ev, room, aid, accounts, buid).await;
            }
        },
    );

    let accounts_for_encrypted = Arc::clone(accounts);
    let account_id_encrypted = account_id.to_string();
    let bot_uid_encrypted = bot_user_id.clone();
    client.add_event_handler(move |ev: OriginalSyncRoomEncryptedEvent, room: Room| {
        let accounts = Arc::clone(&accounts_for_encrypted);
        let aid = account_id_encrypted.clone();
        let buid = bot_uid_encrypted.clone();
        async move {
            handler::handle_room_encrypted_event(ev, room, aid, accounts, buid).await;
        }
    });

    let accounts_for_to_device = Arc::clone(accounts);
    let account_id_to_device = account_id.to_string();
    client.add_event_handler(
        move |ev: matrix_sdk::ruma::events::ToDeviceEvent<
            matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEventContent,
        >| {
            let accounts = Arc::clone(&accounts_for_to_device);
            let aid = account_id_to_device.clone();
            async move {
                verification::handle_to_device_verification_request(ev, aid, accounts).await;
            }
        },
    );

    let accounts_for_poll = Arc::clone(accounts);
    let account_id_poll = account_id.to_string();
    client.add_event_handler(
        move |ev: matrix_sdk::ruma::events::poll::response::OriginalSyncPollResponseEvent,
              room: Room| {
            let accounts = Arc::clone(&accounts_for_poll);
            let aid = account_id_poll.clone();
            let sender_id = ev.sender.to_string();
            let callback_data = handler::first_selection(&ev.content.selections);
            async move {
                handler::handle_poll_response(room, aid, accounts, sender_id, callback_data).await;
            }
        },
    );

    let accounts_for_unstable_poll = Arc::clone(accounts);
    let account_id_unstable_poll = account_id.to_string();
    client.add_event_handler(
        move |ev: matrix_sdk::ruma::events::poll::unstable_response::OriginalSyncUnstablePollResponseEvent,
              room: Room| {
            let accounts = Arc::clone(&accounts_for_unstable_poll);
            let aid = account_id_unstable_poll.clone();
            let sender_id = ev.sender.to_string();
            let callback_data = handler::first_selection(&ev.content.poll_response.answers);
            async move {
                handler::handle_poll_response(room, aid, accounts, sender_id, callback_data).await;
            }
        },
    );

    let accounts_for_invite = Arc::clone(accounts);
    let account_id_invite = account_id.to_string();
    let bot_uid_invite = bot_user_id.clone();
    client.add_event_handler(
        move |ev: matrix_sdk::ruma::events::room::member::StrippedRoomMemberEvent, room: Room| {
            let accounts = Arc::clone(&accounts_for_invite);
            let aid = account_id_invite.clone();
            let buid = bot_uid_invite.clone();
            async move {
                handler::handle_invite(ev, room, aid, accounts, buid).await;
            }
        },
    );
}

#[instrument(skip(client, accounts, cancel), fields(account_id))]
pub(crate) async fn sync_once_and_spawn_loop(
    client: &Client,
    account_id: &str,
    accounts: &AccountStateMap,
    cancel: CancellationToken,
) -> ChannelResult<()> {
    info!(account_id, "performing initial sync...");
    client
        .sync_once(SyncSettings::default())
        .await
        .map_err(|error| ChannelError::external("matrix initial sync", error))?;
    {
        let guard = accounts.read().unwrap_or_else(|error| error.into_inner());
        if let Some(state) = guard.get(account_id) {
            state.mark_initial_sync_complete();
        }
    }
    info!(
        account_id,
        "initial sync complete, starting continuous sync"
    );

    let account_id_for_sync = account_id.to_string();
    let client_for_sync = client.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = client_for_sync.sync(SyncSettings::default()) => {
                warn!(account_id = %account_id_for_sync, "matrix sync loop ended unexpectedly");
            }
            () = cancel.cancelled() => {
                info!(account_id = %account_id_for_sync, "matrix sync loop cancelled");
            }
        }
    });

    Ok(())
}

fn ensure_store_path(account_id: &str) -> ChannelResult<PathBuf> {
    let path = moltis_config::data_dir()
        .join("matrix")
        .join(account_store_component(account_id));
    fs::create_dir_all(&path)
        .map_err(|error| ChannelError::external("matrix create store directory", error))?;
    Ok(path)
}

fn account_store_component(account_id: &str) -> String {
    let sanitized = account_id
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '-',
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    if sanitized.is_empty() {
        "default".to_string()
    } else {
        sanitized
    }
}

fn resolved_device_id(account_id: &str, configured_device_id: Option<&str>) -> String {
    configured_device_id
        .map(str::trim)
        .filter(|device_id| !device_id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("moltis_{}", account_store_component(account_id)))
}

fn configured_device_id(configured_device_id: Option<&str>) -> Option<String> {
    configured_device_id
        .map(str::trim)
        .filter(|device_id| !device_id.is_empty())
        .map(str::to_string)
}

#[instrument(skip(client, config), fields(account_id))]
async fn restore_access_token_session(
    client: &Client,
    account_id: &str,
    config: &MatrixAccountConfig,
) -> ChannelResult<AccessTokenIdentity> {
    let identity = resolve_access_token_identity(config).await?;
    let session = access_token_session(account_id, config, &identity);

    client
        .restore_session(session)
        .await
        .map_err(|error| ChannelError::external("matrix session restore", error))?;

    Ok(identity)
}

fn access_token_session(
    account_id: &str,
    config: &MatrixAccountConfig,
    identity: &AccessTokenIdentity,
) -> matrix_sdk::authentication::matrix::MatrixSession {
    if config.user_id.as_deref().is_some_and(|user_id| {
        let trimmed = user_id.trim();
        !trimmed.is_empty() && trimmed != identity.user_id.as_str()
    }) {
        warn!(
            account_id,
            configured_user_id = config.user_id.as_deref().unwrap_or_default(),
            authenticated_user_id = %identity.user_id,
            "matrix configured user_id does not match token owner, using authenticated user"
        );
    }

    if config.device_id.as_deref().is_some_and(|device_id| {
        let trimmed = device_id.trim();
        identity
            .device_id
            .as_deref()
            .is_some_and(|actual_device_id| !trimmed.is_empty() && trimmed != actual_device_id)
    }) {
        warn!(
            account_id,
            configured_device_id = config.device_id.as_deref().unwrap_or_default(),
            authenticated_device_id = identity.device_id.as_deref().unwrap_or_default(),
            "matrix configured device_id does not match token device, using authenticated device"
        );
    }

    let device_id = identity
        .device_id
        .clone()
        .unwrap_or_else(|| resolved_device_id(account_id, config.device_id.as_deref()));

    matrix_sdk::authentication::matrix::MatrixSession {
        meta: matrix_sdk::SessionMeta {
            user_id: identity.user_id.clone(),
            device_id: device_id.into(),
        },
        tokens: matrix_sdk::SessionTokens {
            access_token: config.access_token.expose_secret().clone(),
            refresh_token: None,
        },
    }
}

#[instrument(skip(config))]
async fn resolve_access_token_identity(
    config: &MatrixAccountConfig,
) -> ChannelResult<AccessTokenIdentity> {
    let homeserver = config.homeserver.trim_end_matches('/');
    let whoami_url = format!("{homeserver}/_matrix/client/v3/account/whoami");
    let response = reqwest::Client::new()
        .get(&whoami_url)
        .bearer_auth(config.access_token.expose_secret())
        .send()
        .await
        .map_err(|error| ChannelError::external("matrix access token whoami", error))?;

    let response = response
        .error_for_status()
        .map_err(|error| match error.status() {
            Some(StatusCode::UNAUTHORIZED) => {
                ChannelError::external("matrix access token whoami", error)
            },
            _ => ChannelError::external("matrix access token whoami", error),
        })?;

    let whoami = response
        .json::<AccessTokenWhoAmIResponse>()
        .await
        .map_err(|error| ChannelError::external("matrix access token whoami decode", error))?;

    Ok(AccessTokenIdentity {
        user_id: whoami.user_id,
        device_id: whoami
            .device_id
            .map(|device_id| device_id.trim().to_string())
            .filter(|device_id| !device_id.is_empty()),
    })
}

#[instrument(skip(client, config), fields(account_id))]
async fn login_with_password(
    client: &Client,
    account_id: &str,
    config: &MatrixAccountConfig,
) -> ChannelResult<()> {
    let user_id = config
        .user_id
        .as_deref()
        .filter(|user_id| !user_id.is_empty())
        .ok_or_else(|| {
            ChannelError::invalid_input("user_id is required when using password authentication")
        })?;
    let password = config
        .password
        .as_ref()
        .map(|secret| secret.expose_secret())
        .ok_or_else(|| ChannelError::invalid_input("password is required"))?;

    let mut login = client.matrix_auth().login_username(user_id, password);
    if let Some(device_id) = configured_device_id(config.device_id.as_deref()) {
        login = login.device_id(&device_id);
    }
    if let Some(display_name) = config
        .device_display_name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        login = login.initial_device_display_name(display_name);
    }

    login
        .send()
        .await
        .map_err(|error| ChannelError::external("matrix password login", error))?;

    info!(account_id, "matrix password login restored session");
    Ok(())
}

#[cfg(test)]
mod tests {
    use {super::*, secrecy::Secret};

    fn config() -> MatrixAccountConfig {
        MatrixAccountConfig {
            homeserver: "https://matrix.example.com".into(),
            ..Default::default()
        }
    }

    #[test]
    fn access_token_auth_is_preferred_when_both_credentials_exist() {
        let cfg = MatrixAccountConfig {
            access_token: Secret::new("syt_test".into()),
            password: Some(Secret::new("wordpass".into())),
            user_id: Some("@bot:example.com".into()),
            ..config()
        };

        assert!(matches!(auth_mode(&cfg), Ok(AuthMode::AccessToken)));
    }

    #[test]
    fn password_auth_is_used_when_token_is_missing() {
        let cfg = MatrixAccountConfig {
            password: Some(Secret::new("wordpass".into())),
            user_id: Some("@bot:example.com".into()),
            ..config()
        };

        assert!(matches!(auth_mode(&cfg), Ok(AuthMode::Password)));
    }

    #[test]
    fn password_auth_requires_user_id() {
        let cfg = MatrixAccountConfig {
            password: Some(Secret::new("wordpass".into())),
            ..config()
        };

        let error = match auth_mode(&cfg) {
            Ok(mode) => panic!("password auth without user_id should fail, got {mode:?}"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("user_id is required"));
    }

    #[test]
    fn authentication_requires_token_or_password() {
        let error = match auth_mode(&config()) {
            Ok(mode) => panic!("missing auth should fail, got {mode:?}"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("either access_token or password is required"));
    }

    #[test]
    fn access_token_session_uses_authenticated_user_and_device_identity() {
        let cfg = MatrixAccountConfig {
            access_token: Secret::new("syt_test".into()),
            user_id: Some("@wrong:example.com".into()),
            device_id: Some("WRONG".into()),
            ..config()
        };
        let actual_user_id = "@bot:example.com"
            .parse()
            .unwrap_or_else(|error| panic!("actual user id should parse: {error}"));
        let identity = AccessTokenIdentity {
            user_id: actual_user_id,
            device_id: Some("ABC123".into()),
        };

        let session = access_token_session("matrix-org", &cfg, &identity);

        assert_eq!(session.meta.user_id.as_str(), "@bot:example.com");
        assert_eq!(session.meta.device_id.as_str(), "ABC123");
    }

    #[test]
    fn access_token_session_falls_back_to_stable_device_id_when_whoami_omits_it() {
        let cfg = MatrixAccountConfig {
            access_token: Secret::new("syt_test".into()),
            ..config()
        };
        let actual_user_id = "@bot:example.com"
            .parse()
            .unwrap_or_else(|error| panic!("actual user id should parse: {error}"));
        let identity = AccessTokenIdentity {
            user_id: actual_user_id,
            device_id: None,
        };

        let session = access_token_session("matrix:org/test bot", &cfg, &identity);

        assert_eq!(session.meta.user_id.as_str(), "@bot:example.com");
        assert_eq!(
            session.meta.device_id.as_str(),
            "moltis_matrix-org-test-bot"
        );
    }

    #[test]
    fn account_store_component_sanitizes_path_segment() {
        assert_eq!(
            account_store_component("matrix-org-lq7m2z"),
            "matrix-org-lq7m2z"
        );
        assert_eq!(
            account_store_component("matrix:org/test bot"),
            "matrix-org-test-bot"
        );
        assert_eq!(account_store_component(":::"), "default");
    }

    #[test]
    fn resolved_device_id_prefers_configured_value() {
        assert_eq!(
            resolved_device_id("matrix-org", Some("MOLTISBOT")),
            "MOLTISBOT"
        );
        assert_eq!(
            resolved_device_id("matrix-org", Some("   ")),
            "moltis_matrix-org"
        );
    }

    #[test]
    fn resolved_device_id_is_stable_without_config() {
        assert_eq!(
            resolved_device_id("matrix:org/test bot", None),
            "moltis_matrix-org-test-bot"
        );
    }

    #[test]
    fn configured_device_id_ignores_blank_values() {
        assert_eq!(
            configured_device_id(Some("MOLTISBOT")),
            Some("MOLTISBOT".into())
        );
        assert_eq!(configured_device_id(Some("   ")), None);
        assert_eq!(configured_device_id(None), None);
    }

    #[test]
    fn encryption_settings_enable_cross_signing_and_key_backfill() {
        let settings = encryption_settings();

        assert!(settings.auto_enable_cross_signing);
        assert_eq!(
            settings.backup_download_strategy,
            BackupDownloadStrategy::AfterDecryptionFailure
        );
    }
}
