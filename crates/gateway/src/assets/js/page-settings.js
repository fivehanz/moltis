// ── Settings page (Preact + HTM + Signals) ───────────────────

import { signal } from "@preact/signals";
import { html } from "htm/preact";
import { render } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import { refresh as refreshGon } from "./gon.js";
import { sendRpc } from "./helpers.js";
import { navigate, registerPrefix } from "./router.js";
import * as S from "./state.js";

var identity = signal(null);
var loading = signal(true);
var activeSection = signal("identity");
var mounted = false;
var containerRef = null;

function rerender() {
	if (containerRef) render(html`<${SettingsPage} />`, containerRef);
}

function fetchIdentity() {
	if (!mounted) return;
	sendRpc("agent.identity.get", {}).then((res) => {
		if (res?.ok) {
			identity.value = res.payload;
			loading.value = false;
			rerender();
		} else if (mounted && !S.connected) {
			setTimeout(fetchIdentity, 500);
		} else {
			loading.value = false;
			rerender();
		}
	});
}

// ── Sidebar navigation items ─────────────────────────────────

var sections = [
	{
		id: "identity",
		label: "Identity",
		icon: html`<svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" width="16" height="16"><path stroke-linecap="round" stroke-linejoin="round" d="M15.75 6a3.75 3.75 0 1 1-7.5 0 3.75 3.75 0 0 1 7.5 0ZM4.501 20.118a7.5 7.5 0 0 1 14.998 0A17.933 17.933 0 0 1 12 21.75c-2.676 0-5.216-.584-7.499-1.632Z"/></svg>`,
	},
	{
		id: "environment",
		label: "Environment",
		icon: html`<svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" width="16" height="16"><path stroke-linecap="round" stroke-linejoin="round" d="m6.75 7.5 3 2.25-3 2.25m4.5 0h3m-9 8.25h13.5A2.25 2.25 0 0 0 21 18V6a2.25 2.25 0 0 0-2.25-2.25H5.25A2.25 2.25 0 0 0 3 6v12a2.25 2.25 0 0 0 2.25 2.25Z"/></svg>`,
	},
	{
		id: "security",
		label: "Security",
		icon: html`<svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" width="16" height="16"><path stroke-linecap="round" stroke-linejoin="round" d="M16.5 10.5V6.75a4.5 4.5 0 1 0-9 0v3.75m-.75 11.25h10.5a2.25 2.25 0 0 0 2.25-2.25v-6.75a2.25 2.25 0 0 0-2.25-2.25H6.75a2.25 2.25 0 0 0-2.25 2.25v6.75a2.25 2.25 0 0 0 2.25 2.25Z"/></svg>`,
	},
];

function SettingsSidebar() {
	return html`<div class="settings-sidebar">
		<div class="settings-sidebar-nav">
			${sections.map(
				(s) => html`
				<button
					key=${s.id}
					class="settings-nav-item ${activeSection.value === s.id ? "active" : ""}"
					onClick=${() => {
						navigate(`/settings/${s.id}`);
					}}
				>
					${s.icon}
					${s.label}
				</button>
			`,
			)}
		</div>
	</div>`;
}

// ── Emoji picker ─────────────────────────────────────────────

var EMOJI_LIST = [
	"\u{1f436}",
	"\u{1f431}",
	"\u{1f43b}",
	"\u{1f43a}",
	"\u{1f981}",
	"\u{1f985}",
	"\u{1f989}",
	"\u{1f427}",
	"\u{1f422}",
	"\u{1f40d}",
	"\u{1f409}",
	"\u{1f984}",
	"\u{1f419}",
	"\u{1f41d}",
	"\u{1f98a}",
	"\u{1f43f}\ufe0f",
	"\u{1f994}",
	"\u{1f987}",
	"\u{1f40a}",
	"\u{1f433}",
	"\u{1f42c}",
	"\u{1f99c}",
	"\u{1f9a9}",
	"\u{1f426}",
	"\u{1f40e}",
	"\u{1f98c}",
	"\u{1f418}",
	"\u{1f99b}",
	"\u{1f43c}",
	"\u{1f428}",
	"\u{1f916}",
	"\u{1f47e}",
	"\u{1f47b}",
	"\u{1f383}",
	"\u{2b50}",
	"\u{1f525}",
	"\u{26a1}",
	"\u{1f308}",
	"\u{1f31f}",
	"\u{1f4a1}",
	"\u{1f52e}",
	"\u{1f680}",
	"\u{1f30d}",
	"\u{1f335}",
	"\u{1f33b}",
	"\u{1f340}",
	"\u{1f344}",
	"\u{2744}\ufe0f",
];

function EmojiPicker({ value, onChange }) {
	var [open, setOpen] = useState(false);
	var wrapRef = useRef(null);

	useEffect(() => {
		if (!open) return;
		function onClick(e) {
			if (wrapRef.current && !wrapRef.current.contains(e.target)) setOpen(false);
		}
		document.addEventListener("mousedown", onClick);
		return () => document.removeEventListener("mousedown", onClick);
	}, [open]);

	return html`<div class="settings-emoji-field" ref=${wrapRef}>
		<input
			type="text"
			class="settings-input"
			style="width:3.5rem;text-align:center;font-size:1.3rem"
			value=${value || ""}
			onInput=${(e) => onChange(e.target.value)}
			placeholder="\u{1f43e}"
		/>
		<button
			type="button"
			class="settings-btn"
			style="padding:0.35rem 0.6rem;font-size:0.75rem"
			onClick=${() => setOpen(!open)}
		>
			${open ? "Close" : "Pick"}
		</button>
		${
			open
				? html`<div class="settings-emoji-picker">
				${EMOJI_LIST.map(
					(em) =>
						html`<button
							type="button"
							class="settings-emoji-btn ${value === em ? "active" : ""}"
							onClick=${() => {
								onChange(em);
								setOpen(false);
							}}
						>
							${em}
						</button>`,
				)}
			</div>`
				: null
		}
	</div>`;
}

// ── Soul defaults ────────────────────────────────────────────

var DEFAULT_SOUL =
	"Be genuinely helpful, not performatively helpful. Skip the filler words \u2014 just help.\n" +
	"Have opinions. You're allowed to disagree, prefer things, find stuff amusing or boring.\n" +
	"Be resourceful before asking. Try to figure it out first \u2014 read the context, search for it \u2014 then ask if you're stuck.\n" +
	"Earn trust through competence. Be careful with external actions. Be bold with internal ones.\n" +
	"Remember you're a guest. You have access to someone's life. Treat it with respect.\n" +
	"Private things stay private. When in doubt, ask before acting externally.\n" +
	"Be concise when needed, thorough when it matters. Not a corporate drone. Not a sycophant. Just good.";

// ── Identity section (editable form) ─────────────────────────

function IdentitySection() {
	var id = identity.value;
	var isNew = !(id && (id.name || id.user_name));

	var [name, setName] = useState(id?.name || "");
	var [emoji, setEmoji] = useState(id?.emoji || "");
	var [creature, setCreature] = useState(id?.creature || "");
	var [vibe, setVibe] = useState(id?.vibe || "");
	var [userName, setUserName] = useState(id?.user_name || "");
	var [soul, setSoul] = useState(id?.soul || "");
	var [saving, setSaving] = useState(false);
	var [saved, setSaved] = useState(false);
	var [error, setError] = useState(null);

	// Sync state when identity loads asynchronously
	useEffect(() => {
		if (!id) return;
		setName(id.name || "");
		setEmoji(id.emoji || "");
		setCreature(id.creature || "");
		setVibe(id.vibe || "");
		setUserName(id.user_name || "");
		setSoul(id.soul || "");
	}, [id]);

	if (loading.value) {
		return html`<div class="settings-content">
			<p class="text-sm text-[var(--muted)]">Loading...</p>
		</div>`;
	}

	function onSave(e) {
		e.preventDefault();
		if (!(name.trim() || userName.trim())) {
			setError("Agent name and your name are required.");
			return;
		}
		if (!name.trim()) {
			setError("Agent name is required.");
			return;
		}
		if (!userName.trim()) {
			setError("Your name is required.");
			return;
		}
		setError(null);
		setSaving(true);
		setSaved(false);

		sendRpc("agent.identity.update", {
			name: name.trim(),
			emoji: emoji.trim() || "",
			creature: creature.trim() || "",
			vibe: vibe.trim() || "",
			soul: soul.trim() || null,
			user_name: userName.trim(),
		}).then((res) => {
			setSaving(false);
			if (res?.ok) {
				identity.value = res.payload;
				refreshGon();
				setSaved(true);
				var banner = document.getElementById("onboardingBanner");
				if (banner) banner.style.display = "none";
				setTimeout(() => {
					setSaved(false);
					rerender();
				}, 2000);
			} else {
				setError(res?.error?.message || "Failed to save");
			}
			rerender();
		});
	}

	function onResetSoul() {
		setSoul("");
		rerender();
	}

	return html`<div class="settings-content">
		<h2 class="settings-title">Identity</h2>
		${
			isNew
				? html`<p class="settings-hint" style="margin-bottom:1rem">
				Welcome! Set up your agent's identity to get started.
			</p>`
				: null
		}
		<form onSubmit=${onSave}>
			<div class="settings-section">
				<h3 class="settings-section-title">Agent</h3>
				<div class="settings-grid">
					<div class="settings-field">
						<label class="settings-label">Name *</label>
						<input
							type="text"
							class="settings-input"
							value=${name}
							onInput=${(e) => setName(e.target.value)}
							placeholder="e.g. Rex"
						/>
					</div>
					<div class="settings-field">
						<label class="settings-label">Emoji</label>
						<${EmojiPicker} value=${emoji} onChange=${setEmoji} />
					</div>
					<div class="settings-field">
						<label class="settings-label">Creature</label>
						<input
							type="text"
							class="settings-input"
							value=${creature}
							onInput=${(e) => setCreature(e.target.value)}
							placeholder="e.g. dog"
						/>
					</div>
					<div class="settings-field">
						<label class="settings-label">Vibe</label>
						<input
							type="text"
							class="settings-input"
							value=${vibe}
							onInput=${(e) => setVibe(e.target.value)}
							placeholder="e.g. chill"
						/>
					</div>
				</div>
			</div>
			<div class="settings-section">
				<h3 class="settings-section-title">User</h3>
				<div class="settings-grid">
					<div class="settings-field">
						<label class="settings-label">Your name *</label>
						<input
							type="text"
							class="settings-input"
							value=${userName}
							onInput=${(e) => setUserName(e.target.value)}
							placeholder="e.g. Alice"
						/>
					</div>
				</div>
			</div>
			<div class="settings-section">
				<h3 class="settings-section-title">Soul</h3>
				<p class="settings-hint">Personality and tone injected into every conversation. Leave empty for the default.</p>
				<textarea
					class="settings-textarea"
					rows="8"
					placeholder=${DEFAULT_SOUL}
					value=${soul}
					onInput=${(e) => setSoul(e.target.value)}
				/>
				${
					soul
						? html`<div style="margin-top:0.25rem">
						<button type="button" class="settings-btn settings-btn-secondary" onClick=${onResetSoul}>Reset to default</button>
					</div>`
						: null
				}
			</div>
			<div class="settings-actions">
				<button type="submit" class="settings-btn" disabled=${saving}>
					${saving ? "Saving\u2026" : "Save"}
				</button>
				${saved ? html`<span class="settings-saved">Saved</span>` : null}
				${error ? html`<span class="settings-error">${error}</span>` : null}
			</div>
		</form>
	</div>`;
}

// ── Environment section ──────────────────────────────────────

function EnvironmentSection() {
	var [envVars, setEnvVars] = useState([]);
	var [envLoading, setEnvLoading] = useState(true);
	var [newKey, setNewKey] = useState("");
	var [newValue, setNewValue] = useState("");
	var [envMsg, setEnvMsg] = useState(null);
	var [envErr, setEnvErr] = useState(null);
	var [saving, setSaving] = useState(false);
	var [updateId, setUpdateId] = useState(null);
	var [updateValue, setUpdateValue] = useState("");

	function fetchEnvVars() {
		fetch("/api/env")
			.then((r) => (r.ok ? r.json() : { env_vars: [] }))
			.then((d) => {
				setEnvVars(d.env_vars || []);
				setEnvLoading(false);
				rerender();
			})
			.catch(() => {
				setEnvLoading(false);
				rerender();
			});
	}

	useEffect(() => {
		fetchEnvVars();
	}, []);

	function onAdd(e) {
		e.preventDefault();
		setEnvErr(null);
		setEnvMsg(null);
		var key = newKey.trim();
		if (!key) {
			setEnvErr("Key is required.");
			rerender();
			return;
		}
		if (!/^[A-Za-z0-9_]+$/.test(key)) {
			setEnvErr("Key must contain only letters, digits, and underscores.");
			rerender();
			return;
		}
		setSaving(true);
		rerender();
		fetch("/api/env", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ key, value: newValue }),
		})
			.then((r) => {
				if (r.ok) {
					setNewKey("");
					setNewValue("");
					setEnvMsg("Variable saved.");
					setTimeout(() => {
						setEnvMsg(null);
						rerender();
					}, 2000);
					fetchEnvVars();
				} else {
					return r.json().then((d) => setEnvErr(d.error || "Failed to save"));
				}
				setSaving(false);
				rerender();
			})
			.catch((err) => {
				setEnvErr(err.message);
				setSaving(false);
				rerender();
			});
	}

	function onDelete(id) {
		fetch(`/api/env/${id}`, { method: "DELETE" }).then(() => fetchEnvVars());
	}

	function onStartUpdate(id) {
		setUpdateId(id);
		setUpdateValue("");
		rerender();
	}

	function onCancelUpdate() {
		setUpdateId(null);
		setUpdateValue("");
		rerender();
	}

	function onConfirmUpdate(key) {
		fetch("/api/env", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ key, value: updateValue }),
		}).then((r) => {
			if (r.ok) {
				setUpdateId(null);
				setUpdateValue("");
				fetchEnvVars();
			}
		});
	}

	return html`<div class="settings-content">
		<h2 class="settings-title">Environment Variables</h2>
		<p class="settings-hint">Environment variables are injected into sandbox command execution. Values are write-only and never displayed.</p>

		${
			envLoading
				? html`<p class="settings-hint">Loading...</p>`
				: html`
			<div class="settings-section">
				${
					envVars.length > 0
						? html`<div class="security-list">
					${envVars.map(
						(v) => html`<div class="security-list-item" key=${v.id}>
						${
							updateId === v.id
								? html`<form style="display:flex;align-items:center;gap:6px;flex:1" onSubmit=${(e) => {
										e.preventDefault();
										onConfirmUpdate(v.key);
									}}>
									<code style="font-size:0.85rem">${v.key}</code>
									<input type="password" class="settings-input" value=${updateValue}
										onInput=${(e) => setUpdateValue(e.target.value)}
										placeholder="New value" style="flex:1" autofocus />
									<button type="submit" class="settings-btn">Save</button>
									<button type="button" class="settings-btn" onClick=${onCancelUpdate}>Cancel</button>
								</form>`
								: html`<div>
									<code style="font-size:0.85rem">${v.key}</code>
									<span style="margin-left:0.5rem;color:var(--muted);font-size:0.78rem">\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022</span>
									<span style="color:var(--muted);font-size:0.78rem"> - <time datetime="${v.updated_at}">${v.updated_at}</time></span>
								</div>
								<div style="display:flex;gap:4px">
									<button class="settings-btn" onClick=${() => onStartUpdate(v.id)}>Update</button>
									<button class="settings-btn settings-btn-danger" onClick=${() => onDelete(v.id)}>Delete</button>
								</div>`
						}
					</div>`,
					)}
				</div>`
						: html`<p class="settings-hint">No environment variables set.</p>`
				}

				<form onSubmit=${onAdd} style="margin-top:1rem">
					<h3 class="settings-section-title">Add Variable</h3>
					<div class="security-add-row" style="flex-wrap:wrap">
						<input type="text" class="settings-input" value=${newKey}
							onInput=${(e) => setNewKey(e.target.value)}
							placeholder="KEY_NAME" style="flex:1;min-width:120px" />
						<input type="password" class="settings-input" value=${newValue}
							onInput=${(e) => setNewValue(e.target.value)}
							placeholder="Value" style="flex:2;min-width:200px" />
						<button type="submit" class="settings-btn" disabled=${saving || !newKey.trim()}>
							${saving ? "Saving\u2026" : "Add"}
						</button>
					</div>
					${envMsg ? html`<span class="settings-saved" style="display:block;margin-top:0.5rem">${envMsg}</span>` : null}
					${envErr ? html`<span class="settings-error" style="display:block;margin-top:0.5rem">${envErr}</span>` : null}
				</form>
			</div>
		`
		}
	</div>`;
}

// ── Security section ─────────────────────────────────────────

function SecuritySection() {
	var [authDisabled, setAuthDisabled] = useState(false);
	var [localhostOnly, setLocalhostOnly] = useState(false);
	var [hasPassword, setHasPassword] = useState(true);
	var [authLoading, setAuthLoading] = useState(true);

	var [curPw, setCurPw] = useState("");
	var [newPw, setNewPw] = useState("");
	var [confirmPw, setConfirmPw] = useState("");
	var [pwMsg, setPwMsg] = useState(null);
	var [pwErr, setPwErr] = useState(null);
	var [pwSaving, setPwSaving] = useState(false);

	var [passkeys, setPasskeys] = useState([]);
	var [pkName, setPkName] = useState("");
	var [pkMsg, setPkMsg] = useState(null);
	var [pkLoading, setPkLoading] = useState(true);
	var [editingPk, setEditingPk] = useState(null);
	var [editingPkName, setEditingPkName] = useState("");

	var [apiKeys, setApiKeys] = useState([]);
	var [akLabel, setAkLabel] = useState("");
	var [akNew, setAkNew] = useState(null);
	var [akLoading, setAkLoading] = useState(true);

	useEffect(() => {
		fetch("/api/auth/status")
			.then((r) => (r.ok ? r.json() : null))
			.then((d) => {
				if (d?.auth_disabled) setAuthDisabled(true);
				if (d?.localhost_only) setLocalhostOnly(true);
				if (d?.has_password === false) setHasPassword(false);
				setAuthLoading(false);
				rerender();
			})
			.catch(() => {
				setAuthLoading(false);
				rerender();
			});
		fetch("/api/auth/passkeys")
			.then((r) => (r.ok ? r.json() : { passkeys: [] }))
			.then((d) => {
				setPasskeys(d.passkeys || []);
				setPkLoading(false);
				rerender();
			})
			.catch(() => setPkLoading(false));
		fetch("/api/auth/api-keys")
			.then((r) => (r.ok ? r.json() : { api_keys: [] }))
			.then((d) => {
				setApiKeys(d.api_keys || []);
				setAkLoading(false);
				rerender();
			})
			.catch(() => setAkLoading(false));
	}, []);

	function onChangePw(e) {
		e.preventDefault();
		setPwErr(null);
		setPwMsg(null);
		if (newPw.length < 8) {
			setPwErr("New password must be at least 8 characters.");
			return;
		}
		if (newPw !== confirmPw) {
			setPwErr("Passwords do not match.");
			return;
		}
		setPwSaving(true);
		var payload = { new_password: newPw };
		if (hasPassword) payload.current_password = curPw;
		fetch("/api/auth/password/change", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify(payload),
		})
			.then((r) => {
				if (r.ok) {
					setPwMsg(hasPassword ? "Password changed." : "Password set.");
					setCurPw("");
					setNewPw("");
					setConfirmPw("");
					setHasPassword(true);
				} else return r.text().then((t) => setPwErr(t));
				setPwSaving(false);
				rerender();
			})
			.catch((err) => {
				setPwErr(err.message);
				setPwSaving(false);
				rerender();
			});
	}

	function onAddPasskey() {
		setPkMsg(null);
		if (/^\d+\.\d+\.\d+\.\d+$/.test(location.hostname) || location.hostname.startsWith("[")) {
			setPkMsg(`Passkeys require a domain name. Use localhost instead of ${location.hostname}`);
			rerender();
			return;
		}
		fetch("/api/auth/passkey/register/begin", { method: "POST" })
			.then((r) => r.json())
			.then((data) => {
				var opts = data.options;
				opts.publicKey.challenge = b64ToBuf(opts.publicKey.challenge);
				opts.publicKey.user.id = b64ToBuf(opts.publicKey.user.id);
				if (opts.publicKey.excludeCredentials) {
					for (var c of opts.publicKey.excludeCredentials) c.id = b64ToBuf(c.id);
				}
				return navigator.credentials
					.create({ publicKey: opts.publicKey })
					.then((cred) => ({ cred, challengeId: data.challenge_id }));
			})
			.then(({ cred, challengeId }) => {
				var body = {
					challenge_id: challengeId,
					name: pkName.trim() || "Passkey",
					credential: {
						id: cred.id,
						rawId: bufToB64(cred.rawId),
						type: cred.type,
						response: {
							attestationObject: bufToB64(cred.response.attestationObject),
							clientDataJSON: bufToB64(cred.response.clientDataJSON),
						},
					},
				};
				return fetch("/api/auth/passkey/register/finish", {
					method: "POST",
					headers: { "Content-Type": "application/json" },
					body: JSON.stringify(body),
				});
			})
			.then((r) => {
				if (r.ok) {
					setPkName("");
					return fetch("/api/auth/passkeys")
						.then((r2) => r2.json())
						.then((d) => {
							setPasskeys(d.passkeys || []);
							setPkMsg("Passkey added.");
							rerender();
						});
				} else
					return r.text().then((t) => {
						setPkMsg(t);
						rerender();
					});
			})
			.catch((err) => {
				setPkMsg(err.message || "Failed to add passkey");
				rerender();
			});
	}

	function onStartRename(id, currentName) {
		setEditingPk(id);
		setEditingPkName(currentName);
		rerender();
	}

	function onCancelRename() {
		setEditingPk(null);
		setEditingPkName("");
		rerender();
	}

	function onConfirmRename(id) {
		var name = editingPkName.trim();
		if (!name) return;
		fetch(`/api/auth/passkeys/${id}`, {
			method: "PATCH",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ name }),
		})
			.then(() => fetch("/api/auth/passkeys").then((r) => r.json()))
			.then((d) => {
				setPasskeys(d.passkeys || []);
				setEditingPk(null);
				setEditingPkName("");
				rerender();
			});
	}

	function onRemovePasskey(id) {
		fetch(`/api/auth/passkeys/${id}`, { method: "DELETE" })
			.then(() => fetch("/api/auth/passkeys").then((r) => r.json()))
			.then((d) => {
				setPasskeys(d.passkeys || []);
				rerender();
			});
	}

	function onCreateApiKey() {
		if (!akLabel.trim()) return;
		setAkNew(null);
		fetch("/api/auth/api-keys", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ label: akLabel.trim() }),
		})
			.then((r) => r.json())
			.then((d) => {
				setAkNew(d.key);
				setAkLabel("");
				return fetch("/api/auth/api-keys").then((r2) => r2.json());
			})
			.then((d) => {
				setApiKeys(d.api_keys || []);
				rerender();
			})
			.catch(() => rerender());
	}

	function onRevokeApiKey(id) {
		fetch(`/api/auth/api-keys/${id}`, { method: "DELETE" })
			.then(() => fetch("/api/auth/api-keys").then((r) => r.json()))
			.then((d) => {
				setApiKeys(d.api_keys || []);
				rerender();
			});
	}

	var [resetConfirm, setResetConfirm] = useState(false);
	var [resetBusy, setResetBusy] = useState(false);

	function onResetAuth() {
		if (!resetConfirm) {
			setResetConfirm(true);
			rerender();
			return;
		}
		setResetBusy(true);
		rerender();
		fetch("/api/auth/reset", { method: "POST" })
			.then((r) => {
				if (r.ok) {
					window.location.reload();
				} else {
					return r.text().then((t) => {
						setPwErr(t);
						setResetConfirm(false);
						setResetBusy(false);
						rerender();
					});
				}
			})
			.catch((err) => {
				setPwErr(err.message);
				setResetConfirm(false);
				setResetBusy(false);
				rerender();
			});
	}

	if (authLoading) {
		return html`<div class="settings-content">
			<h2 class="settings-title">Security</h2>
			<p class="settings-hint">Loading...</p>
		</div>`;
	}

	if (authDisabled) {
		var isScary = !localhostOnly;
		return html`<div class="settings-content">
			<h2 class="settings-title">Security</h2>
			<div class="settings-danger-box" style="margin-top:1rem">
				<strong style="color:var(--error, #e53935)">Authentication is disabled</strong>
				<p class="settings-hint" style="margin-top:0.5rem">
					${
						isScary
							? "Anyone with network access can control moltis and your computer. Set up a password to protect your instance."
							: "Authentication has been removed. While localhost-only access is safe, you should set up a password before exposing moltis to the network."
					}
				</p>
				<button type="button" class="settings-btn" style="margin-top:0.75rem"
					onClick=${() => {
						window.location.href = "/setup";
					}}>Set up authentication</button>
			</div>
		</div>`;
	}

	return html`<div class="settings-content">
		<h2 class="settings-title">Security</h2>

		${
			localhostOnly && !hasPassword
				? html`<div class="settings-info-box" style="margin-top:1rem;margin-bottom:1rem;padding:0.75rem 1rem;border-radius:6px;background:var(--surface2);border:1px solid var(--border)">
			<p class="settings-hint" style="margin:0">
				Moltis is running on localhost, so you have full access without a password.
				Set a password before exposing moltis to the network.
			</p>
		</div>`
				: null
		}

		<div class="settings-section">
			<h3 class="settings-section-title">${hasPassword ? "Change Password" : "Set Password"}</h3>
			<form onSubmit=${onChangePw}>
				<div class="settings-grid">
					${
						hasPassword
							? html`<div class="settings-field">
						<label class="settings-label">Current password</label>
						<input type="password" class="settings-input" value=${curPw}
							onInput=${(e) => setCurPw(e.target.value)} />
					</div>`
							: null
					}
					<div class="settings-field">
						<label class="settings-label">${hasPassword ? "New password" : "Password"}</label>
						<input type="password" class="settings-input" value=${newPw}
							onInput=${(e) => setNewPw(e.target.value)} placeholder="At least 8 characters" />
					</div>
					<div class="settings-field">
						<label class="settings-label">Confirm ${hasPassword ? "new " : ""}password</label>
						<input type="password" class="settings-input" value=${confirmPw}
							onInput=${(e) => setConfirmPw(e.target.value)} />
					</div>
				</div>
				<div class="settings-actions">
					<button type="submit" class="settings-btn" disabled=${pwSaving}>
						${pwSaving ? (hasPassword ? "Changing\u2026" : "Setting\u2026") : hasPassword ? "Change password" : "Set password"}
					</button>
					${pwMsg ? html`<span class="settings-saved">${pwMsg}</span>` : null}
					${pwErr ? html`<span class="settings-error">${pwErr}</span>` : null}
				</div>
			</form>
		</div>

		<div class="settings-section">
			<h3 class="settings-section-title">Passkeys</h3>
			${
				pkLoading
					? html`<p class="text-sm text-[var(--muted)]">Loading...</p>`
					: html`
				${
					passkeys.length > 0
						? html`<div class="security-list">
					${passkeys.map(
						(pk) => html`<div class="security-list-item" key=${pk.id}>
						${
							editingPk === pk.id
								? html`<form style="display:flex;align-items:center;gap:6px;flex:1" onSubmit=${(e) => {
										e.preventDefault();
										onConfirmRename(pk.id);
									}}>
									<input type="text" class="settings-input" value=${editingPkName}
										onInput=${(e) => setEditingPkName(e.target.value)}
										style="flex:1" autofocus />
									<button type="submit" class="settings-btn">Save</button>
									<button type="button" class="settings-btn" onClick=${onCancelRename}>Cancel</button>
								</form>`
								: html`<div>
									<strong>${pk.name}</strong>
									<span style="color:var(--muted);font-size:0.78rem"> - <time datetime="${pk.created_at}">${pk.created_at}</time></span>
								</div>
								<div style="display:flex;gap:4px">
									<button class="settings-btn" onClick=${() => onStartRename(pk.id, pk.name)}>Rename</button>
									<button class="settings-btn settings-btn-danger" onClick=${() => onRemovePasskey(pk.id)}>Remove</button>
								</div>`
						}
					</div>`,
					)}
				</div>`
						: html`<p class="settings-hint">No passkeys registered.</p>`
				}
				<div class="security-add-row">
					<input type="text" class="settings-input" value=${pkName}
						onInput=${(e) => setPkName(e.target.value)}
						placeholder="Passkey name (e.g. MacBook Touch ID)" style="flex:1" />
					<button type="button" class="settings-btn" onClick=${onAddPasskey}>Add passkey</button>
				</div>
				${pkMsg ? html`<p class="settings-hint" style="margin-top:0.5rem">${pkMsg}</p>` : null}
			`
			}
		</div>

		<div class="settings-section">
			<h3 class="settings-section-title">API Keys</h3>
			<p class="settings-hint">API keys authenticate external tools and scripts connecting to moltis over the WebSocket protocol. Pass the key as the <code>api_key</code> field in the <code>auth</code> object of the <code>connect</code> handshake.</p>
			${
				akLoading
					? html`<p class="text-sm text-[var(--muted)]">Loading...</p>`
					: html`
				${
					akNew
						? html`<div class="security-key-reveal">
					<p class="settings-hint">Copy this key now. It won't be shown again.</p>
					<code class="security-key-code">${akNew}</code>
				</div>`
						: null
				}
				${
					apiKeys.length > 0
						? html`<div class="security-list">
					${apiKeys.map(
						(ak) => html`<div class="security-list-item" key=${ak.id}>
						<div>
							<strong>${ak.label}</strong>
							<code style="margin-left:0.5rem;font-size:0.78rem">${ak.key_prefix}...</code>
							<span style="color:var(--muted);font-size:0.78rem"> - <time datetime="${ak.created_at}">${ak.created_at}</time></span>
						</div>
						<button class="settings-btn settings-btn-danger"
							onClick=${() => onRevokeApiKey(ak.id)}>Revoke</button>
					</div>`,
					)}
				</div>`
						: html`<p class="settings-hint">No API keys.</p>`
				}
				<div class="security-add-row">
					<input type="text" class="settings-input" value=${akLabel}
						onInput=${(e) => setAkLabel(e.target.value)}
						placeholder="Key label (e.g. CLI tool)" style="flex:1" />
					<button type="button" class="settings-btn" onClick=${onCreateApiKey} disabled=${!akLabel.trim()}>Generate key</button>
				</div>
			`
			}
		</div>

		<div class="settings-section settings-danger-zone">
			<h3 class="settings-section-title" style="color:var(--danger, #e53935)">Danger Zone</h3>
			<div class="settings-danger-box">
				<div>
					<strong>Remove all authentication</strong>
					<p class="settings-hint" style="margin-top:0.25rem">
						If you know what you're doing, you can fully disable authentication.
						Anyone with network access will be able to access moltis and your computer.
						This removes your password, all passkeys, all API keys, and all sessions.
					</p>
				</div>
				${
					resetConfirm
						? html`<div style="display:flex;align-items:center;gap:8px;margin-top:0.5rem">
						<span class="settings-error" style="margin:0">Are you sure? This cannot be undone.</span>
						<button type="button" class="settings-btn settings-btn-danger" disabled=${resetBusy}
							onClick=${onResetAuth}>${resetBusy ? "Removing\u2026" : "Yes, remove all auth"}</button>
						<button type="button" class="settings-btn" onClick=${() => {
							setResetConfirm(false);
							rerender();
						}}>Cancel</button>
					</div>`
						: html`<button type="button" class="settings-btn settings-btn-danger" style="margin-top:0.5rem"
						onClick=${onResetAuth}>Remove all authentication</button>`
				}
			</div>
		</div>
	</div>`;
}

function b64ToBuf(b64) {
	var str = b64.replace(/-/g, "+").replace(/_/g, "/");
	while (str.length % 4) str += "=";
	var bin = atob(str);
	var buf = new Uint8Array(bin.length);
	for (var i = 0; i < bin.length; i++) buf[i] = bin.charCodeAt(i);
	return buf.buffer;
}

function bufToB64(buf) {
	var bytes = new Uint8Array(buf);
	var str = "";
	for (var b of bytes) str += String.fromCharCode(b);
	return btoa(str).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

// ── Main layout ──────────────────────────────────────────────

function SettingsPage() {
	useEffect(() => {
		fetchIdentity();
	}, []);

	var section = activeSection.value;

	return html`<div class="settings-layout">
		<${SettingsSidebar} />
		${section === "identity" ? html`<${IdentitySection} />` : null}
		${section === "environment" ? html`<${EnvironmentSection} />` : null}
		${section === "security" ? html`<${SecuritySection} />` : null}
	</div>`;
}

registerPrefix(
	"/settings",
	(container, param) => {
		mounted = true;
		containerRef = container;
		container.style.cssText = "flex-direction:row;padding:0;overflow:hidden;";
		var isValidSection = param && sections.some((s) => s.id === param);
		var section = isValidSection ? param : "identity";
		activeSection.value = section;
		if (!isValidSection) {
			history.replaceState(null, "", `/settings/${section}`);
		}
		render(html`<${SettingsPage} />`, container);
		fetchIdentity();
	},
	() => {
		mounted = false;
		if (containerRef) render(null, containerRef);
		containerRef = null;
		identity.value = null;
		loading.value = true;
		activeSection.value = "identity";
	},
);
