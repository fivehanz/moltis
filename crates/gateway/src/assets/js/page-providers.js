// ── Providers page (Preact + HTM + Signals) ─────────────────

import { signal } from "@preact/signals";
import { html } from "htm/preact";
import { render } from "preact";
import { useEffect } from "preact/hooks";
import { sendRpc } from "./helpers.js";
import { fetchModels } from "./models.js";
import { updateNavCount } from "./nav-counts.js";
import { openProviderModal } from "./providers.js";
import { registerPage } from "./router.js";
import { connected } from "./signals.js";
import * as S from "./state.js";
import { ConfirmDialog, requestConfirm } from "./ui.js";

var providers = signal([]);
var loading = signal(false);

function fetchProviders() {
	loading.value = true;
	sendRpc("providers.available", {}).then((res) => {
		loading.value = false;
		if (!res?.ok) return;
		// Filter to configured only and sort: local/ollama first, then alphabetically
		providers.value = (res.payload || [])
			.filter((p) => p.configured)
			.sort((a, b) => {
				// Local and Ollama providers come first
				var aIsLocal = a.authType === "local" || a.name === "ollama";
				var bIsLocal = b.authType === "local" || b.name === "ollama";
				if (aIsLocal && !bIsLocal) return -1;
				if (!aIsLocal && bIsLocal) return 1;
				return a.displayName.localeCompare(b.displayName);
			});
		updateNavCount("providers", providers.value.length);
	});
}

function ProviderCard(props) {
	var p = props.provider;
	var isLocal = p.authType === "local";

	function onRemove() {
		var msg = isLocal ? `Remove ${p.displayName} configuration?` : `Remove credentials for ${p.displayName}?`;
		requestConfirm(msg).then((yes) => {
			if (!yes) return;
			sendRpc("providers.remove_key", { provider: p.name }).then((res) => {
				if (res?.ok) {
					fetchModels();
					fetchProviders();
				}
			});
		});
	}

	function getAuthBadgeText() {
		if (p.authType === "oauth") return "OAuth";
		if (p.authType === "local") return "Local";
		return "API Key";
	}

	// Get model info if available
	var modelInfo = p.model ? p.model : null;
	var endpointInfo = p.baseUrl && p.baseUrl !== p.defaultBaseUrl ? p.baseUrl : null;

	return html`<div class="provider-item" style="margin-bottom:0;cursor:default;">
		<div style="flex:1;min-width:0;">
			<div style="display:flex;align-items:center;gap:8px;">
				<span class="provider-item-name">${p.displayName}</span>
				<span class="provider-item-badge ${p.authType}">
					${getAuthBadgeText()}
				</span>
			</div>
			${
				modelInfo || endpointInfo
					? html`<div style="font-size:.7rem;color:var(--muted);margin-top:4px;display:flex;flex-wrap:wrap;gap:12px;">
					${modelInfo ? html`<span>Model: <span style="font-family:var(--font-mono);">${modelInfo}</span></span>` : null}
					${endpointInfo ? html`<span>Endpoint: <span style="font-family:var(--font-mono);">${endpointInfo}</span></span>` : null}
				</div>`
					: null
			}
		</div>
		<button
			class="provider-btn provider-btn-danger"
			onClick=${onRemove}
		>
			Remove
		</button>
	</div>`;
}

function ProvidersPage() {
	useEffect(() => {
		if (connected.value) fetchProviders();
	}, [connected.value]);

	S.setRefreshProvidersPage(fetchProviders);

	return html`
		<div class="flex-1 flex flex-col min-w-0 p-4 gap-4 overflow-y-auto">
			<h2 class="text-lg font-medium text-[var(--text-strong)]">Providers</h2>
			<p class="text-xs text-[var(--muted)] leading-relaxed max-w-form" style="margin:0;">
				Configure LLM providers for chat and agent tasks. You can add multiple providers and switch between models.
			</p>

			<div style="max-width:600px;">
				${
					loading.value && providers.value.length === 0
						? html`<div class="text-xs text-[var(--muted)]">Loading…</div>`
						: providers.value.length === 0
							? html`<div class="text-xs text-[var(--muted)]" style="padding:12px 0;">No providers configured yet.</div>`
							: html`<div style="display:flex;flex-direction:column;gap:6px;margin-bottom:12px;">
								${providers.value.map((p) => html`<${ProviderCard} key=${p.name} provider=${p} />`)}
							</div>`
				}

				<button
					class="provider-btn"
					onClick=${() => {
						if (connected.value) openProviderModal();
					}}
				>
					Add Provider
				</button>
			</div>
		</div>
		<${ConfirmDialog} />
	`;
}

registerPage(
	"/providers",
	function initProviders(container) {
		container.style.cssText = "flex-direction:column;padding:0;overflow:hidden;";
		render(html`<${ProvidersPage} />`, container);
	},
	function teardownProviders() {
		S.setRefreshProvidersPage(null);
		var container = S.$("pageContent");
		if (container) render(null, container);
	},
);
