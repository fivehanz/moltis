// ── Provider modal ──────────────────────────────────────

import { onEvent } from "./events.js";
import { sendRpc } from "./helpers.js";
import { ensureProviderModal } from "./modals.js";
import { fetchModels } from "./models.js";
import * as S from "./state.js";

var _els = null;

function els() {
	if (!_els) {
		ensureProviderModal();
		_els = {
			modal: S.$("providerModal"),
			body: S.$("providerModalBody"),
			title: S.$("providerModalTitle"),
			close: S.$("providerModalClose"),
		};
		_els.close.addEventListener("click", closeProviderModal);
		_els.modal.addEventListener("click", (e) => {
			if (e.target === _els.modal) closeProviderModal();
		});
	}
	return _els;
}

// Re-export for backwards compat with page-providers.js
export function getProviderModal() {
	return els().modal;
}

// Providers that support custom endpoint configuration
var OPENAI_COMPATIBLE_PROVIDERS = [
	"openai",
	"mistral",
	"openrouter",
	"cerebras",
	"minimax",
	"moonshot",
	"venice",
	"ollama",
];

export function openProviderModal() {
	var m = els();
	m.modal.classList.remove("hidden");
	m.title.textContent = "Add Provider";
	m.body.textContent = "Loading...";
	sendRpc("providers.available", {}).then((res) => {
		if (!res?.ok) {
			m.body.textContent = "Failed to load providers.";
			return;
		}
		var providers = res.payload || [];

		// Sort: local/ollama first, then alphabetically
		providers.sort((a, b) => {
			var aIsLocal = a.authType === "local" || a.name === "ollama";
			var bIsLocal = b.authType === "local" || b.name === "ollama";
			if (aIsLocal && !bIsLocal) return -1;
			if (!aIsLocal && bIsLocal) return 1;
			return a.displayName.localeCompare(b.displayName);
		});

		m.body.textContent = "";
		providers.forEach((p) => {
			var item = document.createElement("div");
			// Don't gray out configured providers - users can add multiple
			item.className = "provider-item";
			var name = document.createElement("span");
			name.className = "provider-item-name";
			name.textContent = p.displayName;
			item.appendChild(name);

			var badges = document.createElement("div");
			badges.className = "badge-row";

			if (p.configured) {
				var check = document.createElement("span");
				check.className = "provider-item-badge configured";
				check.textContent = "configured";
				badges.appendChild(check);
			}

			var badge = document.createElement("span");
			badge.className = `provider-item-badge ${p.authType}`;
			if (p.authType === "oauth") {
				badge.textContent = "OAuth";
			} else if (p.authType === "local") {
				badge.textContent = "Local";
			} else {
				badge.textContent = "API Key";
			}
			badges.appendChild(badge);
			item.appendChild(badges);

			item.addEventListener("click", () => {
				if (p.authType === "api-key") showApiKeyForm(p);
				else if (p.authType === "oauth") showOAuthFlow(p);
				else if (p.authType === "local") showLocalModelFlow(p);
			});
			m.body.appendChild(item);
		});
	});
}

export function closeProviderModal() {
	els().modal.classList.add("hidden");
}

export function showApiKeyForm(provider) {
	var m = els();
	m.title.textContent = provider.displayName;
	m.body.textContent = "";

	var form = document.createElement("div");
	form.className = "provider-key-form";

	// Check if this provider supports custom endpoint
	var supportsEndpoint = OPENAI_COMPATIBLE_PROVIDERS.includes(provider.name);

	// API Key field
	var keyLabel = document.createElement("label");
	keyLabel.className = "text-xs text-[var(--muted)]";
	keyLabel.textContent = "API Key";
	form.appendChild(keyLabel);

	var keyInp = document.createElement("input");
	keyInp.className = "provider-key-input";
	keyInp.type = "password";
	keyInp.placeholder = provider.name === "ollama" ? "(optional for Ollama)" : "sk-...";
	form.appendChild(keyInp);

	// Endpoint field for OpenAI-compatible providers
	var endpointInp = null;
	if (supportsEndpoint) {
		var endpointLabel = document.createElement("label");
		endpointLabel.className = "text-xs text-[var(--muted)]";
		endpointLabel.style.marginTop = "8px";
		endpointLabel.textContent = "Endpoint (optional)";
		form.appendChild(endpointLabel);

		endpointInp = document.createElement("input");
		endpointInp.className = "provider-key-input";
		endpointInp.type = "text";
		endpointInp.placeholder = provider.defaultBaseUrl || "https://api.example.com/v1";
		form.appendChild(endpointInp);

		var hint = document.createElement("div");
		hint.className = "text-xs text-[var(--muted)]";
		hint.style.marginTop = "2px";
		hint.textContent = "Leave empty to use the default endpoint.";
		form.appendChild(hint);
	}

	// Model field for bring-your-own-model providers
	var modelInp = null;
	var needsModel = provider.name === "ollama" || provider.name === "openrouter" || provider.name === "venice";
	if (needsModel) {
		var modelLabel = document.createElement("label");
		modelLabel.className = "text-xs text-[var(--muted)]";
		modelLabel.style.marginTop = "8px";
		modelLabel.textContent = "Model ID";
		form.appendChild(modelLabel);

		modelInp = document.createElement("input");
		modelInp.className = "provider-key-input";
		modelInp.type = "text";
		modelInp.placeholder = provider.name === "ollama" ? "llama3" : "model-id";
		form.appendChild(modelInp);
	}

	var btns = document.createElement("div");
	btns.className = "btn-row";
	btns.style.marginTop = "12px";

	var backBtn = document.createElement("button");
	backBtn.className = "provider-btn provider-btn-secondary";
	backBtn.textContent = "Back";
	backBtn.addEventListener("click", openProviderModal);
	btns.appendChild(backBtn);

	var saveBtn = document.createElement("button");
	saveBtn.className = "provider-btn";
	saveBtn.textContent = "Save";
	saveBtn.addEventListener("click", () => {
		var key = keyInp.value.trim();
		// Ollama doesn't require a key
		if (!key && provider.name !== "ollama") return;

		// Model is required for bring-your-own providers
		if (needsModel && modelInp && !modelInp.value.trim()) {
			keyLabel.textContent = "Model ID is required";
			keyLabel.classList.add("text-error");
			return;
		}

		saveBtn.disabled = true;
		saveBtn.textContent = "Saving...";

		var payload = {
			provider: provider.name,
			apiKey: key || "ollama", // Use dummy key for Ollama
		};
		if (endpointInp?.value.trim()) {
			payload.baseUrl = endpointInp.value.trim();
		}
		if (modelInp?.value.trim()) {
			payload.model = modelInp.value.trim();
		}

		sendRpc("providers.save_key", payload).then((res) => {
			if (res?.ok) {
				m.body.textContent = "";
				var status = document.createElement("div");
				status.className = "provider-status";
				status.textContent = `${provider.displayName} configured successfully!`;
				m.body.appendChild(status);
				fetchModels();
				if (S.refreshProvidersPage) S.refreshProvidersPage();
				setTimeout(closeProviderModal, 1500);
			} else {
				saveBtn.disabled = false;
				saveBtn.textContent = "Save";
				var err = res?.error?.message || "Failed to save";
				keyLabel.textContent = err;
				keyLabel.classList.add("text-error");
			}
		});
	});
	btns.appendChild(saveBtn);
	form.appendChild(btns);
	m.body.appendChild(form);
	keyInp.focus();
}

export function showOAuthFlow(provider) {
	var m = els();
	m.title.textContent = provider.displayName;
	m.body.textContent = "";

	var wrapper = document.createElement("div");
	wrapper.className = "provider-key-form";

	var desc = document.createElement("div");
	desc.className = "text-xs text-[var(--muted)]";
	desc.textContent = `Click below to authenticate with ${provider.displayName} via OAuth.`;
	wrapper.appendChild(desc);

	var btns = document.createElement("div");
	btns.className = "btn-row";

	var backBtn = document.createElement("button");
	backBtn.className = "provider-btn provider-btn-secondary";
	backBtn.textContent = "Back";
	backBtn.addEventListener("click", openProviderModal);
	btns.appendChild(backBtn);

	var connectBtn = document.createElement("button");
	connectBtn.className = "provider-btn";
	connectBtn.textContent = "Connect";
	connectBtn.addEventListener("click", () => {
		connectBtn.disabled = true;
		connectBtn.textContent = "Starting...";
		sendRpc("providers.oauth.start", { provider: provider.name }).then((res) => {
			if (res?.ok && res.payload && res.payload.authUrl) {
				window.open(res.payload.authUrl, "_blank");
				connectBtn.textContent = "Waiting for auth...";
				pollOAuthStatus(provider);
			} else if (res?.ok && res.payload && res.payload.deviceFlow) {
				connectBtn.textContent = "Waiting for auth...";
				desc.classList.remove("text-error");
				desc.textContent = "";
				var linkEl = document.createElement("a");
				linkEl.href = res.payload.verificationUri;
				linkEl.target = "_blank";
				linkEl.className = "oauth-link";
				linkEl.textContent = res.payload.verificationUri;
				var codeEl = document.createElement("strong");
				codeEl.textContent = res.payload.userCode;
				desc.appendChild(document.createTextNode("Go to "));
				desc.appendChild(linkEl);
				desc.appendChild(document.createTextNode(" and enter code: "));
				desc.appendChild(codeEl);
				pollOAuthStatus(provider);
			} else {
				connectBtn.disabled = false;
				connectBtn.textContent = "Connect";
				desc.textContent = res?.error?.message || "Failed to start OAuth";
				desc.classList.add("text-error");
			}
		});
	});
	btns.appendChild(connectBtn);
	wrapper.appendChild(btns);
	m.body.appendChild(wrapper);
}

function pollOAuthStatus(provider) {
	var m = els();
	var attempts = 0;
	var maxAttempts = 60;
	var timer = setInterval(() => {
		attempts++;
		if (attempts > maxAttempts) {
			clearInterval(timer);
			m.body.textContent = "";
			var timeout = document.createElement("div");
			timeout.className = "text-xs text-[var(--error)]";
			timeout.textContent = "OAuth timed out. Please try again.";
			m.body.appendChild(timeout);
			return;
		}
		sendRpc("providers.oauth.status", { provider: provider.name }).then((res) => {
			if (res?.ok && res.payload && res.payload.authenticated) {
				clearInterval(timer);
				m.body.textContent = "";
				var status = document.createElement("div");
				status.className = "provider-status";
				status.textContent = `${provider.displayName} connected successfully!`;
				m.body.appendChild(status);
				fetchModels();
				if (S.refreshProvidersPage) S.refreshProvidersPage();
				setTimeout(closeProviderModal, 1500);
			}
		});
	}, 2000);
}

// ── Local model flow ──────────────────────────────────────

export function showLocalModelFlow(provider) {
	var m = els();
	m.title.textContent = provider.displayName;
	m.body.textContent = "Loading system info...";

	// Fetch system info first
	sendRpc("providers.local.system_info", {}).then((sysRes) => {
		if (!sysRes?.ok) {
			m.body.textContent = sysRes?.error?.message || "Failed to get system info";
			return;
		}
		var sysInfo = sysRes.payload;

		// Fetch available models
		sendRpc("providers.local.models", {}).then((modelsRes) => {
			if (!modelsRes?.ok) {
				m.body.textContent = modelsRes?.error?.message || "Failed to get models";
				return;
			}
			var modelsData = modelsRes.payload;
			renderLocalModelSelection(provider, sysInfo, modelsData);
		});
	});
}

// Store the selected backend for model configuration
var selectedBackend = null;

function renderLocalModelSelection(provider, sysInfo, modelsData) {
	var m = els();
	m.body.textContent = "";

	// Initialize selected backend to recommended
	selectedBackend = sysInfo.recommendedBackend || "GGUF";

	var wrapper = document.createElement("div");
	wrapper.className = "provider-key-form";

	// System info section
	var sysSection = document.createElement("div");
	sysSection.className = "flex flex-col gap-2 mb-4";

	var sysTitle = document.createElement("div");
	sysTitle.className = "text-xs font-medium text-[var(--text-strong)]";
	sysTitle.textContent = "System Info";
	sysSection.appendChild(sysTitle);

	var sysDetails = document.createElement("div");
	sysDetails.className = "flex gap-3 text-xs text-[var(--muted)]";

	var ramSpan = document.createElement("span");
	ramSpan.textContent = `RAM: ${sysInfo.totalRamGb}GB`;
	sysDetails.appendChild(ramSpan);

	var tierSpan = document.createElement("span");
	tierSpan.textContent = `Tier: ${sysInfo.memoryTier}`;
	sysDetails.appendChild(tierSpan);

	if (sysInfo.hasGpu) {
		var gpuSpan = document.createElement("span");
		gpuSpan.className = "text-[var(--ok)]";
		gpuSpan.textContent = "GPU available";
		sysDetails.appendChild(gpuSpan);
	}

	sysSection.appendChild(sysDetails);
	wrapper.appendChild(sysSection);

	// Backend selector (show on Apple Silicon where both GGUF and MLX are options)
	var backends = sysInfo.availableBackends || [];
	if (sysInfo.isAppleSilicon && backends.length > 0) {
		var backendSection = document.createElement("div");
		backendSection.className = "flex flex-col gap-2 mb-4";

		var backendLabel = document.createElement("div");
		backendLabel.className = "text-xs font-medium text-[var(--text-strong)]";
		backendLabel.textContent = "Inference Backend";
		backendSection.appendChild(backendLabel);

		var backendCards = document.createElement("div");
		backendCards.className = "flex flex-col gap-2";

		backends.forEach((b) => {
			var card = document.createElement("div");
			card.className = "backend-card";
			if (!b.available) card.className += " disabled";
			if (b.id === selectedBackend) card.className += " selected";
			card.dataset.backendId = b.id;

			var header = document.createElement("div");
			header.className = "flex items-center justify-between";

			var name = document.createElement("span");
			name.className = "backend-name text-sm font-medium text-[var(--text)]";
			name.textContent = b.name;
			header.appendChild(name);

			var badges = document.createElement("div");
			badges.className = "flex gap-2";

			if (b.id === sysInfo.recommendedBackend && b.available) {
				var recBadge = document.createElement("span");
				recBadge.className = "recommended-badge";
				recBadge.textContent = "Recommended";
				badges.appendChild(recBadge);
			}

			if (!b.available) {
				var unavailBadge = document.createElement("span");
				unavailBadge.className = "tier-badge";
				unavailBadge.textContent = "Not installed";
				badges.appendChild(unavailBadge);
			}

			header.appendChild(badges);
			card.appendChild(header);

			var desc = document.createElement("div");
			desc.className = "text-xs text-[var(--muted)] mt-1";
			desc.textContent = b.description;
			card.appendChild(desc);

			// Show install instructions for unavailable backends
			if (!b.available && b.id === "MLX") {
				var installHint = document.createElement("div");
				installHint.className = "install-hint";
				installHint.innerHTML = "Install with: <code>pip install mlx-lm</code>";
				card.appendChild(installHint);
			}

			if (b.available) {
				card.addEventListener("click", () => {
					// Deselect all cards
					backendCards.querySelectorAll(".backend-card").forEach((c) => {
						c.classList.remove("selected");
					});
					// Select this card
					card.classList.add("selected");
					selectedBackend = b.id;
				});
			}

			backendCards.appendChild(card);
		});

		backendSection.appendChild(backendCards);
		wrapper.appendChild(backendSection);
	} else if (sysInfo.backendNote) {
		// Non-Apple Silicon - just show info
		var backendDiv = document.createElement("div");
		backendDiv.className = "text-xs text-[var(--muted)] mb-4";
		backendDiv.innerHTML = `<span class="font-medium">Backend:</span> ${sysInfo.backendNote}`;
		wrapper.appendChild(backendDiv);
	}

	// Models section
	var modelsTitle = document.createElement("div");
	modelsTitle.className = "text-xs font-medium text-[var(--text-strong)] mb-2";
	modelsTitle.textContent = "Select a Model";
	wrapper.appendChild(modelsTitle);

	var modelsList = document.createElement("div");
	modelsList.className = "flex flex-col gap-2 max-h-64 overflow-y-auto";

	var recommended = modelsData.recommended || [];
	recommended.forEach((model) => {
		var card = createModelCard(model, provider);
		modelsList.appendChild(card);
	});

	wrapper.appendChild(modelsList);

	// Back button
	var btns = document.createElement("div");
	btns.className = "btn-row mt-4";

	var backBtn = document.createElement("button");
	backBtn.className = "provider-btn provider-btn-secondary";
	backBtn.textContent = "Back";
	backBtn.addEventListener("click", openProviderModal);
	btns.appendChild(backBtn);
	wrapper.appendChild(btns);

	m.body.appendChild(wrapper);
}

function createModelCard(model, provider) {
	var card = document.createElement("div");
	card.className = "model-card";

	var header = document.createElement("div");
	header.className = "flex items-center justify-between";

	var name = document.createElement("span");
	name.className = "text-sm font-medium text-[var(--text)]";
	name.textContent = model.displayName;
	header.appendChild(name);

	var badges = document.createElement("div");
	badges.className = "flex gap-2";

	var ramBadge = document.createElement("span");
	ramBadge.className = "tier-badge";
	ramBadge.textContent = `${model.minRamGb}GB`;
	badges.appendChild(ramBadge);

	if (model.suggested) {
		var suggestedBadge = document.createElement("span");
		suggestedBadge.className = "recommended-badge";
		suggestedBadge.textContent = "Recommended";
		badges.appendChild(suggestedBadge);
	}

	header.appendChild(badges);
	card.appendChild(header);

	var meta = document.createElement("div");
	meta.className = "text-xs text-[var(--muted)] mt-1";
	meta.textContent = `Context: ${(model.contextWindow / 1000).toFixed(0)}k tokens`;
	card.appendChild(meta);

	card.addEventListener("click", () => selectLocalModel(model, provider));

	return card;
}

function selectLocalModel(model, provider) {
	var m = els();
	m.body.textContent = "";

	var wrapper = document.createElement("div");
	wrapper.className = "provider-key-form";

	var status = document.createElement("div");
	status.className = "text-sm text-[var(--text)]";
	status.textContent = `Configuring ${model.displayName}...`;
	wrapper.appendChild(status);

	var progress = document.createElement("div");
	progress.className = "download-progress mt-4";

	var progressBar = document.createElement("div");
	progressBar.className = "download-progress-bar";
	progressBar.style.width = "0%";
	progress.appendChild(progressBar);

	var progressText = document.createElement("div");
	progressText.className = "text-xs text-[var(--muted)] mt-2";
	progress.appendChild(progressText);

	wrapper.appendChild(progress);
	m.body.appendChild(wrapper);

	// Subscribe to download progress events
	var off = onEvent("local-llm.download", (payload) => {
		if (payload.modelId !== model.id) return;

		if (payload.error) {
			status.textContent = payload.error;
			status.className = "text-sm text-[var(--error)]";
			off();
			return;
		}

		if (payload.complete) {
			status.textContent = `${model.displayName} downloaded successfully!`;
			status.className = "provider-status";
			progressBar.style.width = "100%";
			progressText.textContent = "";
			off();
			fetchModels();
			if (S.refreshProvidersPage) S.refreshProvidersPage();
			setTimeout(closeProviderModal, 1500);
			return;
		}

		// Update progress
		if (payload.progress != null) {
			progressBar.style.width = `${payload.progress.toFixed(1)}%`;
			status.textContent = `Downloading ${model.displayName}...`;
		}
		if (payload.downloaded != null) {
			var downloadedMb = (payload.downloaded / (1024 * 1024)).toFixed(1);
			if (payload.total != null) {
				var totalMb = (payload.total / (1024 * 1024)).toFixed(1);
				progressText.textContent = `${downloadedMb} MB / ${totalMb} MB`;
			} else {
				progressText.textContent = `${downloadedMb} MB downloaded`;
			}
		}
	});

	sendRpc("providers.local.configure", { modelId: model.id, backend: selectedBackend }).then((res) => {
		if (!res?.ok) {
			status.textContent = res?.error?.message || "Failed to configure model";
			status.className = "text-sm text-[var(--error)]";
			off(); // Unsubscribe from events
			return;
		}

		// Start polling for status as a fallback (in case WebSocket events are missed)
		pollLocalStatus(model, provider, status, progress, off);
	});
}

function pollLocalStatus(model, _provider, statusEl, progressEl, offEvent) {
	var attempts = 0;
	var maxAttempts = 300; // 10 minutes with 2s interval
	var completed = false;
	var timer = setInterval(() => {
		if (completed) {
			clearInterval(timer);
			return;
		}
		attempts++;
		if (attempts > maxAttempts) {
			clearInterval(timer);
			if (offEvent) offEvent();
			statusEl.textContent = "Configuration timed out. Please try again.";
			statusEl.className = "text-sm text-[var(--error)]";
			return;
		}

		sendRpc("providers.local.status", {}).then((res) => {
			if (!res?.ok) return;
			var st = res.payload;

			if (st.status === "ready" || st.status === "loaded") {
				completed = true;
				clearInterval(timer);
				if (offEvent) offEvent();
				statusEl.textContent = `${model.displayName} configured successfully!`;
				statusEl.className = "provider-status";
				progressEl.style.display = "none";
				fetchModels();
				if (S.refreshProvidersPage) S.refreshProvidersPage();
				setTimeout(closeProviderModal, 1500);
			} else if (st.status === "error") {
				completed = true;
				clearInterval(timer);
				if (offEvent) offEvent();
				statusEl.textContent = st.error || "Configuration failed";
				statusEl.className = "text-sm text-[var(--error)]";
			}
			// Don't update progress here - let WebSocket events handle it
		});
	}, 2000);
}
