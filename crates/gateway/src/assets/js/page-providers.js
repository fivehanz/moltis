// ── Providers page ───────────────────────────────────────
import * as S from "./state.js";
import { sendRpc, createEl } from "./helpers.js";
import { registerPage } from "./router.js";
import { openProviderModal } from "./providers.js";
import { fetchModels } from "./models.js";

// Safe: static hardcoded HTML template string — no user input is interpolated.
var providersPageHTML =
  '<div class="flex-1 flex flex-col min-w-0 p-4 gap-4 overflow-y-auto">' +
    '<div class="flex items-center gap-3">' +
      '<h2 class="text-lg font-medium text-[var(--text-strong)]">Providers</h2>' +
      '<button id="provAddBtn" class="bg-[var(--accent-dim)] text-white border-none px-3 py-1.5 rounded text-xs cursor-pointer hover:bg-[var(--accent)] transition-colors">+ Add Provider</button>' +
    '</div>' +
    '<div id="providerPageList"></div>' +
  '</div>';

registerPage("/providers", function initProviders(container) {
  container.innerHTML = providersPageHTML; // safe: static template, no user input

  var addBtn = S.$("provAddBtn");
  var listEl = S.$("providerPageList");

  addBtn.addEventListener("click", function () {
    if (S.connected) openProviderModal();
  });

  function renderProviderList() {
    sendRpc("providers.available", {}).then(function (res) {
      if (!res || !res.ok) return;
      var providers = (res.payload || []).filter(function (p) {
        return p.configured;
      }).sort(function (a, b) {
        return a.displayName.localeCompare(b.displayName);
      });
      while (listEl.firstChild) listEl.removeChild(listEl.firstChild);

      if (providers.length === 0) {
        listEl.appendChild(createEl("div", {
          className: "text-sm text-[var(--muted)]",
          textContent: "No providers connected yet."
        }));
        return;
      }

      providers.forEach(function (p) {
        var card = createEl("div", {
          style: "display:flex;align-items:center;justify-content:space-between;padding:10px 12px;border:1px solid var(--border);border-radius:6px;margin-bottom:6px;"
        });

        var left = createEl("div", { style: "display:flex;align-items:center;gap:8px;" });
        left.appendChild(createEl("span", {
          className: "text-sm text-[var(--text-strong)]",
          textContent: p.displayName
        }));

        var badge = createEl("span", {
          className: "provider-item-badge " + p.authType,
          textContent: p.authType === "oauth" ? "OAuth" : "API Key"
        });
        left.appendChild(badge);

        card.appendChild(left);

        var removeBtn = createEl("button", {
          className: "session-action-btn session-delete",
          textContent: "Remove",
          title: "Remove " + p.displayName
        });
        removeBtn.addEventListener("click", function () {
          if (!confirm("Remove credentials for " + p.displayName + "?")) return;
          sendRpc("providers.remove_key", { provider: p.name }).then(function (res) {
            if (res && res.ok) {
              fetchModels();
              renderProviderList();
            }
          });
        });
        card.appendChild(removeBtn);

        listEl.appendChild(card);
      });
    });
  }

  S.setRefreshProvidersPage(renderProviderList);
  renderProviderList();
}, function teardownProviders() {
  S.setRefreshProvidersPage(null);
});
