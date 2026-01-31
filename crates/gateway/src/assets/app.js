(function () {
  "use strict";

  var $ = function (id) { return document.getElementById(id); };

  // ── Shared state ──────────────────────────────────────────────
  var ws = null;
  var reqId = 0;
  var connected = false;
  var reconnectDelay = 1000;
  var pending = {};
  var models = [];
  var activeSessionKey = localStorage.getItem("moltis-session") || "main";
  var activeProjectId = localStorage.getItem("moltis-project") || "";
  var sessions = [];
  var projects = [];

  // Chat-page specific state (persists across page transitions)
  var streamEl = null;
  var streamText = "";
  var lastToolOutput = "";
  var chatHistory = JSON.parse(localStorage.getItem("moltis-chat-history") || "[]");
  var chatHistoryIdx = -1; // -1 = not browsing history
  var chatHistoryDraft = "";

  // Session token usage tracking (cumulative for the current session)
  var sessionTokens = { input: 0, output: 0 };

  // ── Theme ────────────────────────────────────────────────────
  function getSystemTheme() {
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }

  function applyTheme(mode) {
    var resolved = mode === "system" ? getSystemTheme() : mode;
    document.documentElement.setAttribute("data-theme", resolved);
    document.documentElement.style.colorScheme = resolved;
    updateThemeButtons(mode);
  }

  function updateThemeButtons(activeMode) {
    var buttons = document.querySelectorAll(".theme-btn");
    buttons.forEach(function (btn) {
      btn.classList.toggle("active", btn.getAttribute("data-theme-val") === activeMode);
    });
  }

  function initTheme() {
    var saved = localStorage.getItem("moltis-theme") || "system";
    applyTheme(saved);
    window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", function () {
      var current = localStorage.getItem("moltis-theme") || "system";
      if (current === "system") applyTheme("system");
    });
    $("themeToggle").addEventListener("click", function (e) {
      var btn = e.target.closest(".theme-btn");
      if (!btn) return;
      var mode = btn.getAttribute("data-theme-val");
      localStorage.setItem("moltis-theme", mode);
      applyTheme(mode);
    });
  }
  initTheme();

  // ── Helpers ──────────────────────────────────────────────────
  function nextId() { return "ui-" + (++reqId); }

  var dot = $("statusDot");
  var sText = $("statusText");
  // Model selector elements — created dynamically inside the chat page
  var modelCombo = null;
  var modelComboBtn = null;
  var modelComboLabel = null;
  var modelDropdown = null;
  var modelSearchInput = null;
  var modelDropdownList = null;
  var selectedModelId = localStorage.getItem("moltis-model") || "";
  var modelIdx = -1;
  function setSessionModel(sessionKey, modelId) {
    sendRpc("sessions.patch", { key: sessionKey, model: modelId });
  }

  // ── Session project combo (in chat header) ──────────────────
  var projectCombo = null;
  var projectComboBtn = null;
  var projectComboLabel = null;
  var projectDropdown = null;
  var projectDropdownList = null;

  function openProjectDropdown() {
    if (!projectDropdown) return;
    projectDropdown.classList.remove("hidden");
    renderProjectDropdownList();
  }

  function closeProjectDropdown() {
    if (!projectDropdown) return;
    projectDropdown.classList.add("hidden");
  }

  function renderProjectDropdownList() {
    if (!projectDropdownList) return;
    projectDropdownList.textContent = "";
    // "No project" option
    var none = document.createElement("div");
    none.className = "model-dropdown-item" + (!activeProjectId ? " selected" : "");
    var noneLabel = document.createElement("span");
    noneLabel.className = "model-item-label";
    noneLabel.textContent = "No project";
    none.appendChild(noneLabel);
    none.addEventListener("click", function () { selectProject("", "No project"); });
    projectDropdownList.appendChild(none);
    (projects || []).forEach(function (p) {
      var el = document.createElement("div");
      el.className = "model-dropdown-item" + (p.id === activeProjectId ? " selected" : "");
      var lbl = document.createElement("span");
      lbl.className = "model-item-label";
      lbl.textContent = p.label || p.id;
      el.appendChild(lbl);
      el.addEventListener("click", function () { selectProject(p.id, p.label || p.id); });
      projectDropdownList.appendChild(el);
    });
  }

  function selectProject(id, label) {
    activeProjectId = id;
    localStorage.setItem("moltis-project", activeProjectId);
    if (projectComboLabel) projectComboLabel.textContent = label;
    closeProjectDropdown();
    if (connected && activeSessionKey) {
      sendRpc("sessions.patch", { key: activeSessionKey, project_id: id });
    }
  }

  function updateSessionProjectSelect(projectId) {
    if (!projectComboLabel) return;
    if (!projectId) {
      projectComboLabel.textContent = "No project";
      return;
    }
    var proj = (projects || []).find(function (p) { return p.id === projectId; });
    projectComboLabel.textContent = proj ? (proj.label || proj.id) : projectId;
  }

  function renderSessionProjectSelect() {
    updateSessionProjectSelect(activeProjectId);
  }

  function bindProjectComboEvents() {
    if (!projectComboBtn || !projectCombo) return;
    projectComboBtn.addEventListener("click", function () {
      if (projectDropdown.classList.contains("hidden")) {
        openProjectDropdown();
      } else {
        closeProjectDropdown();
      }
    });
  }

  document.addEventListener("click", function (e) {
    if (projectCombo && !projectCombo.contains(e.target)) {
      closeProjectDropdown();
    }
  });

  // ── Sandbox toggle ───────────────────────────────────────────
  var sandboxToggleBtn = null;
  var sandboxLabel = null;
  var sessionSandboxEnabled = true;

  function updateSandboxUI(enabled) {
    sessionSandboxEnabled = !!enabled;
    if (!sandboxLabel || !sandboxToggleBtn) return;
    if (sessionSandboxEnabled) {
      sandboxLabel.textContent = "sandboxed";
      sandboxToggleBtn.style.borderColor = "var(--accent, #f59e0b)";
      sandboxToggleBtn.style.color = "var(--accent, #f59e0b)";
    } else {
      sandboxLabel.textContent = "direct";
      sandboxToggleBtn.style.borderColor = "";
      sandboxToggleBtn.style.color = "var(--muted)";
    }
  }

  function bindSandboxToggleEvents() {
    if (!sandboxToggleBtn) return;
    sandboxToggleBtn.addEventListener("click", function () {
      var newVal = !sessionSandboxEnabled;
      sendRpc("sessions.patch", { key: activeSessionKey, sandbox_enabled: newVal }).then(function (res) {
        if (res && res.result) {
          updateSandboxUI(res.result.sandbox_enabled);
        } else {
          updateSandboxUI(newVal);
        }
      });
    });
  }
  var sessionsPanel = $("sessionsPanel");
  var sessionList = $("sessionList");
  var newSessionBtn = $("newSessionBtn");

  function setStatus(state, text) {
    dot.className = "status-dot " + state;
    sText.textContent = text;
    var sendBtn = $("sendBtn");
    if (sendBtn) sendBtn.disabled = state !== "connected";
  }

  function esc(s) {
    return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
  }

  function renderMarkdown(raw) {
    // Input is escaped via esc() before calling this, so the resulting
    // HTML only contains tags we explicitly create (pre, code, strong).
    var s = esc(raw);
    s = s.replace(/```(\w*)\n([\s\S]*?)```/g, function (_, lang, code) {
      return "<pre><code>" + code + "</code></pre>";
    });
    s = s.replace(/`([^`]+)`/g, "<code>$1</code>");
    s = s.replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>");
    return s;
  }

  function sendRpc(method, params) {
    return new Promise(function (resolve) {
      var id = nextId();
      pending[id] = resolve;
      ws.send(JSON.stringify({ type: "req", id: id, method: method, params: params }));
    });
  }

  function fetchModels() {
    sendRpc("models.list", {}).then(function (res) {
      if (!res || !res.ok) return;
      models = res.payload || [];
      if (models.length === 0) return;
      var saved = localStorage.getItem("moltis-model") || "";
      var found = models.find(function (m) { return m.id === saved; });
      if (found) {
        selectedModelId = found.id;
        if (modelComboLabel) modelComboLabel.textContent = found.displayName || found.id;
      } else {
        selectedModelId = models[0].id;
        if (modelComboLabel) modelComboLabel.textContent = models[0].displayName || models[0].id;
        localStorage.setItem("moltis-model", selectedModelId);
      }
    });
  }

  function selectModel(m) {
    selectedModelId = m.id;
    if (modelComboLabel) modelComboLabel.textContent = m.displayName || m.id;
    localStorage.setItem("moltis-model", m.id);
    setSessionModel(activeSessionKey, m.id);
    closeModelDropdown();
  }

  function openModelDropdown() {
    if (!modelDropdown) return;
    modelDropdown.classList.remove("hidden");
    modelSearchInput.value = "";
    modelIdx = -1;
    renderModelList("");
    requestAnimationFrame(function () { if (modelSearchInput) modelSearchInput.focus(); });
  }

  function closeModelDropdown() {
    if (!modelDropdown) return;
    modelDropdown.classList.add("hidden");
    if (modelSearchInput) modelSearchInput.value = "";
    modelIdx = -1;
  }

  function renderModelList(query) {
    if (!modelDropdownList) return;
    modelDropdownList.textContent = "";
    var q = query.toLowerCase();
    var filtered = models.filter(function (m) {
      var label = (m.displayName || m.id).toLowerCase();
      var provider = (m.provider || "").toLowerCase();
      return !q || label.indexOf(q) !== -1 || provider.indexOf(q) !== -1 || m.id.toLowerCase().indexOf(q) !== -1;
    });
    if (filtered.length === 0) {
      var empty = document.createElement("div");
      empty.className = "model-dropdown-empty";
      empty.textContent = "No matching models";
      modelDropdownList.appendChild(empty);
      return;
    }
    filtered.forEach(function (m, i) {
      var el = document.createElement("div");
      el.className = "model-dropdown-item";
      if (m.id === selectedModelId) el.classList.add("selected");
      var label = document.createElement("span");
      label.className = "model-item-label";
      label.textContent = m.displayName || m.id;
      el.appendChild(label);
      if (m.provider) {
        var prov = document.createElement("span");
        prov.className = "model-item-provider";
        prov.textContent = m.provider;
        el.appendChild(prov);
      }
      el.addEventListener("click", function () { selectModel(m); });
      modelDropdownList.appendChild(el);
    });
  }

  function updateModelActive() {
    if (!modelDropdownList) return;
    var items = modelDropdownList.querySelectorAll(".model-dropdown-item");
    items.forEach(function (el, i) {
      el.classList.toggle("kb-active", i === modelIdx);
    });
    if (modelIdx >= 0 && items[modelIdx]) {
      items[modelIdx].scrollIntoView({ block: "nearest" });
    }
  }

  // Model combo event listeners are set up dynamically inside initChat
  // when the model selector is created in the chat page.
  function bindModelComboEvents() {
    if (!modelComboBtn || !modelSearchInput || !modelDropdownList || !modelCombo) return;

    modelComboBtn.addEventListener("click", function () {
      if (modelDropdown.classList.contains("hidden")) {
        openModelDropdown();
      } else {
        closeModelDropdown();
      }
    });

    modelSearchInput.addEventListener("input", function () {
      modelIdx = -1;
      renderModelList(modelSearchInput.value.trim());
    });

    modelSearchInput.addEventListener("keydown", function (e) {
      var items = modelDropdownList.querySelectorAll(".model-dropdown-item");
      if (e.key === "ArrowDown") {
        e.preventDefault();
        modelIdx = Math.min(modelIdx + 1, items.length - 1);
        updateModelActive();
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        modelIdx = Math.max(modelIdx - 1, 0);
        updateModelActive();
      } else if (e.key === "Enter") {
        e.preventDefault();
        if (modelIdx >= 0 && items[modelIdx]) {
          items[modelIdx].click();
        } else if (items.length === 1) {
          items[0].click();
        }
      } else if (e.key === "Escape") {
        closeModelDropdown();
        modelComboBtn.focus();
      }
    });
  }

  document.addEventListener("click", function (e) {
    if (modelCombo && !modelCombo.contains(e.target)) {
      closeModelDropdown();
    }
  });

  // ── Router ──────────────────────────────────────────────────
  var pages = {};
  var currentPage = null;
  var pageContent = $("pageContent");

  function registerPage(path, init, teardown) {
    pages[path] = { init: init, teardown: teardown || function () {} };
  }

  function navigate(path) {
    if (path === currentPage) return;
    history.pushState(null, "", path);
    mount(path);
  }

  function mount(path) {
    if (currentPage && pages[currentPage]) {
      pages[currentPage].teardown();
    }
    pageContent.textContent = "";

    var page = pages[path] || pages["/"];
    currentPage = pages[path] ? path : "/";

    var links = document.querySelectorAll(".nav-link");
    links.forEach(function (a) {
      a.classList.toggle("active", a.getAttribute("href") === currentPage);
    });

    // Show sessions panel only on the chat page
    if (currentPage === "/") {
      sessionsPanel.classList.remove("hidden");
    } else {
      sessionsPanel.classList.add("hidden");
    }

    // Clear unseen logs alert when viewing the logs page
    if (currentPage === "/logs") clearLogsAlert();

    if (page) page.init(pageContent);
  }

  window.addEventListener("popstate", function () {
    mount(location.pathname);
  });

  // ── Nav panel (burger toggle) ────────────────────────────────
  var burgerBtn = $("burgerBtn");
  var navPanel = $("navPanel");

  burgerBtn.addEventListener("click", function () {
    navPanel.classList.toggle("hidden");
  });

  navPanel.addEventListener("click", function (e) {
    var link = e.target.closest("[data-nav]");
    if (!link) return;
    e.preventDefault();
    navigate(link.getAttribute("href"));
  });

  function fetchSessions() {
    sendRpc("sessions.list", {}).then(function (res) {
      if (!res || !res.ok) return;
      sessions = res.payload || [];
      renderSessionList();
    });
  }

  function renderSessionList() {
    sessionList.textContent = "";
    var filtered = sessions;
    if (projectFilterId) {
      filtered = sessions.filter(function (s) {
        return s.projectId === projectFilterId;
      });
    }
    filtered.forEach(function (s) {
      var item = document.createElement("div");
      item.className = "session-item" + (s.key === activeSessionKey ? " active" : "");
      item.setAttribute("data-session-key", s.key);

      var info = document.createElement("div");
      info.className = "session-info";

      var label = document.createElement("div");
      label.className = "session-label";
      label.textContent = s.label || s.key;
      info.appendChild(label);

      var meta = document.createElement("div");
      meta.className = "session-meta";
      meta.setAttribute("data-session-key", s.key);
      var count = s.messageCount || 0;
      var metaText = count + " msg" + (count !== 1 ? "s" : "");
      if (s.worktree_branch) {
        metaText += " \u00b7 \u2387 " + s.worktree_branch;
      }
      meta.textContent = metaText;
      info.appendChild(meta);

      item.appendChild(info);

      var actions = document.createElement("div");
      actions.className = "session-actions";

      if (s.key !== "main") {
        var renameBtn = document.createElement("button");
        renameBtn.className = "session-action-btn";
        renameBtn.textContent = "\u270F";
        renameBtn.title = "Rename";
        renameBtn.addEventListener("click", function (e) {
          e.stopPropagation();
          var newLabel = prompt("Rename session:", s.label || s.key);
          if (newLabel !== null) {
            sendRpc("sessions.patch", { key: s.key, label: newLabel }).then(fetchSessions);
          }
        });
        actions.appendChild(renameBtn);

        var deleteBtn = document.createElement("button");
        deleteBtn.className = "session-action-btn session-delete";
        deleteBtn.textContent = "\u2715";
        deleteBtn.title = "Delete";
        deleteBtn.addEventListener("click", function (e) {
          e.stopPropagation();
          var metaEl = sessionList.querySelector('.session-meta[data-session-key="' + s.key + '"]');
          var count = metaEl ? (parseInt(metaEl.textContent, 10) || 0) : (s.messageCount || 0);
          if (count > 0 && !confirm("Delete this session?")) return;
          sendRpc("sessions.delete", { key: s.key }).then(function (res) {
            if (res && !res.ok && res.error && res.error.indexOf("uncommitted changes") !== -1) {
              if (confirm("Worktree has uncommitted changes. Force delete?")) {
                sendRpc("sessions.delete", { key: s.key, force: true }).then(function () {
                  if (activeSessionKey === s.key) switchSession("main");
                  fetchSessions();
                });
              }
              return;
            }
            if (activeSessionKey === s.key) switchSession("main");
            fetchSessions();
          });
        });
        actions.appendChild(deleteBtn);
      }
      item.appendChild(actions);

      item.addEventListener("click", function () {
        if (currentPage !== "/") navigate("/");
        switchSession(s.key);
      });

      sessionList.appendChild(item);
    });
  }

  function setSessionReplying(key, replying) {
    var el = sessionList.querySelector('.session-item[data-session-key="' + key + '"]');
    if (el) el.classList.toggle("replying", replying);
  }

  function setSessionUnread(key, unread) {
    var el = sessionList.querySelector('.session-item[data-session-key="' + key + '"]');
    if (el) el.classList.toggle("unread", unread);
  }

  function bumpSessionCount(key, increment) {
    var el = sessionList.querySelector('.session-meta[data-session-key="' + key + '"]');
    if (!el) return;
    var current = parseInt(el.textContent, 10) || 0;
    var next = current + increment;
    el.textContent = next + " msg" + (next !== 1 ? "s" : "");
  }

  newSessionBtn.addEventListener("click", function () {
    if (currentPage !== "/") navigate("/");
    var key = "session:" + crypto.randomUUID();
    switchSession(key, null, null);
  });

  // ── Projects ──────────────────────────────────────────────────
  var projectSelect = $("projectSelect");

  function fetchProjects() {
    sendRpc("projects.list", {}).then(function (res) {
      if (!res || !res.ok) return;
      projects = res.payload || [];
      renderProjectSelect();
      renderSessionProjectSelect();
    });
  }

  function renderProjectSelect() {
    // Clear existing options safely
    while (projectSelect.firstChild) projectSelect.removeChild(projectSelect.firstChild);
    var defaultOpt = document.createElement("option");
    defaultOpt.value = "";
    defaultOpt.textContent = "All sessions";
    projectSelect.appendChild(defaultOpt);

    projects.forEach(function (p) {
      var opt = document.createElement("option");
      opt.value = p.id;
      opt.textContent = p.label || p.id;
      projectSelect.appendChild(opt);
    });
    projectSelect.value = projectFilterId || "";
  }

  var projectFilterId = localStorage.getItem("moltis-project-filter") || "";

  projectSelect.addEventListener("change", function () {
    projectFilterId = projectSelect.value;
    localStorage.setItem("moltis-project-filter", projectFilterId);
    renderSessionList();
  });

  // ── Project modal ─────────────────────────────────────────────
  var projectModal = $("projectModal");
  var projectModalBody = $("projectModalBody");
  var projectModalClose = $("projectModalClose");
  var manageProjectsBtn = $("manageProjectsBtn");

  manageProjectsBtn.addEventListener("click", function () {
    renderProjectModal();
    projectModal.classList.remove("hidden");
  });

  projectModalClose.addEventListener("click", function () {
    projectModal.classList.add("hidden");
  });

  projectModal.addEventListener("click", function (e) {
    if (e.target === projectModal) projectModal.classList.add("hidden");
  });

  function renderProjectModal() {
    // Clear safely
    while (projectModalBody.firstChild) projectModalBody.removeChild(projectModalBody.firstChild);

    // Detect button
    var detectBtn = document.createElement("button");
    detectBtn.className = "provider-btn provider-btn-secondary";
    detectBtn.textContent = "Auto-detect projects";
    detectBtn.style.marginBottom = "8px";
    detectBtn.addEventListener("click", function () {
      detectBtn.disabled = true;
      detectBtn.textContent = "Detecting...";
      // Use home directory as a starting point
      sendRpc("projects.detect", { directories: [] }).then(function (res) {
        detectBtn.disabled = false;
        detectBtn.textContent = "Auto-detect projects";
        if (res && res.ok) {
          fetchProjects();
          renderProjectModal();
        }
      });
    });
    projectModalBody.appendChild(detectBtn);

    // Add project form
    var addForm = document.createElement("div");
    addForm.className = "provider-key-form";
    addForm.style.marginBottom = "12px";

    var dirLabel = document.createElement("div");
    dirLabel.className = "text-xs text-[var(--muted)]";
    dirLabel.textContent = "Add project by directory path:";
    addForm.appendChild(dirLabel);

    var dirWrap = document.createElement("div");
    dirWrap.style.position = "relative";

    var dirInput = document.createElement("input");
    dirInput.type = "text";
    dirInput.className = "provider-key-input";
    dirInput.placeholder = "/path/to/project";
    dirInput.style.fontFamily = "var(--font-mono)";
    dirWrap.appendChild(dirInput);

    var completionList = document.createElement("div");
    completionList.style.cssText = "position:absolute;left:0;right:0;top:100%;background:var(--surface);border:1px solid var(--border);border-radius:4px;max-height:150px;overflow-y:auto;z-index:20;display:none;";
    dirWrap.appendChild(completionList);
    addForm.appendChild(dirWrap);

    var addBtnRow = document.createElement("div");
    addBtnRow.style.display = "flex";
    addBtnRow.style.gap = "8px";

    var addBtn = document.createElement("button");
    addBtn.className = "provider-btn";
    addBtn.textContent = "Add project";
    addBtn.addEventListener("click", function () {
      var dir = dirInput.value.trim();
      if (!dir) return;
      addBtn.disabled = true;
      // Detect from this specific directory
      sendRpc("projects.detect", { directories: [dir] }).then(function (res) {
        addBtn.disabled = false;
        if (res && res.ok) {
          var detected = res.payload || [];
          if (detected.length === 0) {
            // Not a git repo — create manually
            var slug = dir.split("/").filter(Boolean).pop() || "project";
            var now = Date.now();
            sendRpc("projects.upsert", {
              id: slug.toLowerCase().replace(/[^a-z0-9-]/g, "-"),
              label: slug,
              directory: dir,
              auto_worktree: false,
              detected: false,
              created_at: now,
              updated_at: now
            }).then(function () {
              fetchProjects();
              renderProjectModal();
            });
          } else {
            fetchProjects();
            renderProjectModal();
          }
        }
      });
    });
    addBtnRow.appendChild(addBtn);
    addForm.appendChild(addBtnRow);
    projectModalBody.appendChild(addForm);

    // Directory autocomplete
    var completeTimer = null;
    dirInput.addEventListener("input", function () {
      clearTimeout(completeTimer);
      completeTimer = setTimeout(function () {
        var val = dirInput.value;
        if (val.length < 2) { completionList.style.display = "none"; return; }
        sendRpc("projects.complete_path", { partial: val }).then(function (res) {
          if (!res || !res.ok) { completionList.style.display = "none"; return; }
          var paths = res.payload || [];
          while (completionList.firstChild) completionList.removeChild(completionList.firstChild);
          if (paths.length === 0) { completionList.style.display = "none"; return; }
          paths.forEach(function (p) {
            var item = document.createElement("div");
            item.textContent = p;
            item.style.cssText = "padding:6px 10px;cursor:pointer;font-size:.78rem;font-family:var(--font-mono);color:var(--text);transition:background .1s;";
            item.addEventListener("mouseenter", function () { item.style.background = "var(--bg-hover)"; });
            item.addEventListener("mouseleave", function () { item.style.background = ""; });
            item.addEventListener("click", function () {
              dirInput.value = p + "/";
              completionList.style.display = "none";
              dirInput.focus();
              // Trigger another completion for the subdirectory
              dirInput.dispatchEvent(new Event("input"));
            });
            completionList.appendChild(item);
          });
          completionList.style.display = "block";
        });
      }, 200);
    });

    // Separator
    var sep = document.createElement("div");
    sep.style.cssText = "border-top:1px solid var(--border);margin:4px 0 8px;";
    projectModalBody.appendChild(sep);

    // Existing projects list
    if (projects.length === 0) {
      var empty = document.createElement("div");
      empty.className = "text-xs text-[var(--muted)]";
      empty.textContent = "No projects configured yet.";
      projectModalBody.appendChild(empty);
    } else {
      projects.forEach(function (p) {
        var row = document.createElement("div");
        row.className = "provider-item";

        var info = document.createElement("div");
        info.style.flex = "1";
        info.style.minWidth = "0";

        var name = document.createElement("div");
        name.className = "provider-item-name";
        name.textContent = p.label || p.id;
        info.appendChild(name);

        var dir = document.createElement("div");
        dir.style.cssText = "font-size:.7rem;color:var(--muted);font-family:var(--font-mono);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;";
        dir.textContent = p.directory;
        info.appendChild(dir);

        row.appendChild(info);

        var actions = document.createElement("div");
        actions.style.cssText = "display:flex;gap:4px;flex-shrink:0;";

        if (p.detected) {
          var badge = document.createElement("span");
          badge.className = "provider-item-badge api-key";
          badge.textContent = "auto";
          actions.appendChild(badge);
        }

        var delBtn = document.createElement("button");
        delBtn.className = "session-action-btn session-delete";
        delBtn.textContent = "x";
        delBtn.title = "Remove project";
        delBtn.addEventListener("click", function (e) {
          e.stopPropagation();
          sendRpc("projects.delete", { id: p.id }).then(function () {
            fetchProjects();
            renderProjectModal();
          });
        });
        actions.appendChild(delBtn);

        row.appendChild(actions);

        // Click to select
        row.addEventListener("click", function () {
          activeProjectId = p.id;
          localStorage.setItem("moltis-project", activeProjectId);
          renderProjectSelect();
          projectModal.classList.add("hidden");
        });

        projectModalBody.appendChild(row);
      });
    }
  }

  // ── Session search ──────────────────────────────────────────
  var searchInput = $("sessionSearch");
  var searchResults = $("searchResults");
  var searchTimer = null;
  var searchHits = [];
  var searchIdx = -1;

  function debounceSearch() {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(doSearch, 300);
  }

  function doSearch() {
    var q = searchInput.value.trim();
    if (!q || !connected) { hideSearch(); return; }
    sendRpc("sessions.search", { query: q }).then(function (res) {
      if (!res || !res.ok) { hideSearch(); return; }
      searchHits = res.payload || [];
      searchIdx = -1;
      renderSearchResults(q);
    });
  }

  function hideSearch() {
    searchResults.classList.add("hidden");
    searchHits = [];
    searchIdx = -1;
  }

  function renderSearchResults(query) {
    searchResults.textContent = "";
    if (searchHits.length === 0) {
      var empty = document.createElement("div");
      empty.style.padding = "8px 10px";
      empty.style.fontSize = ".78rem";
      empty.style.color = "var(--muted)";
      empty.textContent = "No results";
      searchResults.appendChild(empty);
      searchResults.classList.remove("hidden");
      return;
    }
    searchHits.forEach(function (hit, i) {
      var el = document.createElement("div");
      el.className = "search-hit";
      el.setAttribute("data-idx", i);

      var lbl = document.createElement("div");
      lbl.className = "search-hit-label";
      lbl.textContent = hit.label || hit.sessionKey;
      el.appendChild(lbl);

      // Safe: esc() escapes all HTML entities first, then we only wrap
      // the already-escaped query substring in <mark> tags.
      var snip = document.createElement("div");
      snip.className = "search-hit-snippet";
      var escaped = esc(hit.snippet);
      var qEsc = esc(query);
      var re = new RegExp("(" + qEsc.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + ")", "gi");
      snip.innerHTML = escaped.replace(re, "<mark>$1</mark>");
      el.appendChild(snip);

      var role = document.createElement("div");
      role.className = "search-hit-role";
      role.textContent = hit.role;
      el.appendChild(role);

      el.addEventListener("click", function () {
        if (currentPage !== "/") navigate("/");
        var ctx = { query: query, messageIndex: hit.messageIndex };
        switchSession(hit.sessionKey, ctx);
        searchInput.value = "";
        hideSearch();
      });

      searchResults.appendChild(el);
    });
    searchResults.classList.remove("hidden");
  }

  function updateSearchActive() {
    var items = searchResults.querySelectorAll(".search-hit");
    items.forEach(function (el, i) {
      el.classList.toggle("kb-active", i === searchIdx);
    });
    if (searchIdx >= 0 && items[searchIdx]) {
      items[searchIdx].scrollIntoView({ block: "nearest" });
    }
  }

  searchInput.addEventListener("input", debounceSearch);
  searchInput.addEventListener("keydown", function (e) {
    if (searchResults.classList.contains("hidden")) return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      searchIdx = Math.min(searchIdx + 1, searchHits.length - 1);
      updateSearchActive();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      searchIdx = Math.max(searchIdx - 1, 0);
      updateSearchActive();
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (searchIdx >= 0 && searchHits[searchIdx]) {
        var h = searchHits[searchIdx];
        if (currentPage !== "/") navigate("/");
        var ctx = { query: searchInput.value.trim(), messageIndex: h.messageIndex };
        switchSession(h.sessionKey, ctx);
        searchInput.value = "";
        hideSearch();
      }
    } else if (e.key === "Escape") {
      searchInput.value = "";
      hideSearch();
    }
  });

  document.addEventListener("click", function (e) {
    if (!searchInput.contains(e.target) && !searchResults.contains(e.target)) {
      hideSearch();
    }
  });

  // ── Provider modal ──────────────────────────────────────────
  var providerModal = $("providerModal");
  var providerModalBody = $("providerModalBody");
  var providerModalTitle = $("providerModalTitle");
  var providerModalClose = $("providerModalClose");

  function openProviderModal() {
    providerModal.classList.remove("hidden");
    providerModalTitle.textContent = "Add Provider";
    providerModalBody.textContent = "Loading...";
    sendRpc("providers.available", {}).then(function (res) {
      if (!res || !res.ok) {
        providerModalBody.textContent = "Failed to load providers.";
        return;
      }
      var providers = res.payload || [];
      providerModalBody.textContent = "";
      providers.forEach(function (p) {
        var item = document.createElement("div");
        item.className = "provider-item" + (p.configured ? " configured" : "");
        var name = document.createElement("span");
        name.className = "provider-item-name";
        name.textContent = p.displayName;
        item.appendChild(name);

        var badges = document.createElement("div");
        badges.style.display = "flex";
        badges.style.gap = "6px";
        badges.style.alignItems = "center";

        if (p.configured) {
          var check = document.createElement("span");
          check.className = "provider-item-badge configured";
          check.textContent = "configured";
          badges.appendChild(check);
        }

        var badge = document.createElement("span");
        badge.className = "provider-item-badge " + p.authType;
        badge.textContent = p.authType === "oauth" ? "OAuth" : "API Key";
        badges.appendChild(badge);
        item.appendChild(badges);

        item.addEventListener("click", function () {
          if (p.authType === "api-key") showApiKeyForm(p);
          else if (p.authType === "oauth") showOAuthFlow(p);
        });
        providerModalBody.appendChild(item);
      });
    });
  }

  function closeProviderModal() {
    providerModal.classList.add("hidden");
  }

  function showApiKeyForm(provider) {
    providerModalTitle.textContent = provider.displayName;
    providerModalBody.textContent = "";

    var form = document.createElement("div");
    form.className = "provider-key-form";

    var label = document.createElement("label");
    label.className = "text-xs text-[var(--muted)]";
    label.textContent = "API Key";
    form.appendChild(label);

    var inp = document.createElement("input");
    inp.className = "provider-key-input";
    inp.type = "password";
    inp.placeholder = "sk-...";
    form.appendChild(inp);

    var btns = document.createElement("div");
    btns.style.display = "flex";
    btns.style.gap = "8px";

    var backBtn = document.createElement("button");
    backBtn.className = "provider-btn provider-btn-secondary";
    backBtn.textContent = "Back";
    backBtn.addEventListener("click", openProviderModal);
    btns.appendChild(backBtn);

    var saveBtn = document.createElement("button");
    saveBtn.className = "provider-btn";
    saveBtn.textContent = "Save";
    saveBtn.addEventListener("click", function () {
      var key = inp.value.trim();
      if (!key) return;
      saveBtn.disabled = true;
      saveBtn.textContent = "Saving...";
      sendRpc("providers.save_key", { provider: provider.name, apiKey: key }).then(function (res) {
        if (res && res.ok) {
          providerModalBody.textContent = "";
          var status = document.createElement("div");
          status.className = "provider-status";
          status.textContent = provider.displayName + " configured successfully!";
          providerModalBody.appendChild(status);
          fetchModels();
          if (refreshProvidersPage) refreshProvidersPage();
          setTimeout(closeProviderModal, 1500);
        } else {
          saveBtn.disabled = false;
          saveBtn.textContent = "Save";
          var err = (res && res.error && res.error.message) || "Failed to save";
          inp.style.borderColor = "var(--error)";
          label.textContent = err;
          label.style.color = "var(--error)";
        }
      });
    });
    btns.appendChild(saveBtn);
    form.appendChild(btns);
    providerModalBody.appendChild(form);
    inp.focus();
  }

  function showOAuthFlow(provider) {
    providerModalTitle.textContent = provider.displayName;
    providerModalBody.textContent = "";

    var wrapper = document.createElement("div");
    wrapper.className = "provider-key-form";

    var desc = document.createElement("div");
    desc.className = "text-xs text-[var(--muted)]";
    desc.textContent = "Click below to authenticate with " + provider.displayName + " via OAuth.";
    wrapper.appendChild(desc);

    var btns = document.createElement("div");
    btns.style.display = "flex";
    btns.style.gap = "8px";

    var backBtn = document.createElement("button");
    backBtn.className = "provider-btn provider-btn-secondary";
    backBtn.textContent = "Back";
    backBtn.addEventListener("click", openProviderModal);
    btns.appendChild(backBtn);

    var connectBtn = document.createElement("button");
    connectBtn.className = "provider-btn";
    connectBtn.textContent = "Connect";
    connectBtn.addEventListener("click", function () {
      connectBtn.disabled = true;
      connectBtn.textContent = "Starting...";
      sendRpc("providers.oauth.start", { provider: provider.name }).then(function (res) {
        if (res && res.ok && res.payload && res.payload.authUrl) {
          window.open(res.payload.authUrl, "_blank");
          connectBtn.textContent = "Waiting for auth...";
          pollOAuthStatus(provider);
        } else if (res && res.ok && res.payload && res.payload.deviceFlow) {
          connectBtn.textContent = "Waiting for auth...";
          desc.style.color = "";
          desc.textContent = "";
          var linkEl = document.createElement("a");
          linkEl.href = res.payload.verificationUri;
          linkEl.target = "_blank";
          linkEl.style.color = "var(--accent)";
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
          desc.textContent = (res && res.error && res.error.message) || "Failed to start OAuth";
          desc.style.color = "var(--error)";
        }
      });
    });
    btns.appendChild(connectBtn);
    wrapper.appendChild(btns);
    providerModalBody.appendChild(wrapper);
  }

  function pollOAuthStatus(provider) {
    var attempts = 0;
    var maxAttempts = 60;
    var timer = setInterval(function () {
      attempts++;
      if (attempts > maxAttempts) {
        clearInterval(timer);
        providerModalBody.textContent = "";
        var timeout = document.createElement("div");
        timeout.className = "text-xs text-[var(--error)]";
        timeout.textContent = "OAuth timed out. Please try again.";
        providerModalBody.appendChild(timeout);
        return;
      }
      sendRpc("providers.oauth.status", { provider: provider.name }).then(function (res) {
        if (res && res.ok && res.payload && res.payload.authenticated) {
          clearInterval(timer);
          providerModalBody.textContent = "";
          var status = document.createElement("div");
          status.className = "provider-status";
          status.textContent = provider.displayName + " connected successfully!";
          providerModalBody.appendChild(status);
          fetchModels();
          if (refreshProvidersPage) refreshProvidersPage();
          setTimeout(closeProviderModal, 1500);
        }
      });
    }, 2000);
  }

  providerModalClose.addEventListener("click", closeProviderModal);
  providerModal.addEventListener("click", function (e) {
    if (e.target === providerModal) closeProviderModal();
  });

  var refreshProvidersPage = null;

  // ── Error helpers ───────────────────────────────────────────
  function parseErrorMessage(message) {
    var jsonMatch = message.match(/\{[\s\S]*\}$/);
    if (jsonMatch) {
      try {
        var err = JSON.parse(jsonMatch[0]);
        var errObj = err.error || err;
        if (errObj.type === "usage_limit_reached" || (errObj.message && errObj.message.indexOf("usage limit") !== -1)) {
          return { icon: "", title: "Usage limit reached", detail: "Your " + (errObj.plan_type || "current") + " plan limit has been reached.", resetsAt: errObj.resets_at ? errObj.resets_at * 1000 : null };
        }
        if (errObj.type === "rate_limit_exceeded" || (errObj.message && errObj.message.indexOf("rate limit") !== -1)) {
          return { icon: "\u26A0\uFE0F", title: "Rate limited", detail: errObj.message || "Too many requests. Please wait a moment.", resetsAt: errObj.resets_at ? errObj.resets_at * 1000 : null };
        }
        if (errObj.message) {
          return { icon: "\u26A0\uFE0F", title: "Error", detail: errObj.message, resetsAt: null };
        }
      } catch (e) { /* fall through */ }
    }
    var statusMatch = message.match(/HTTP (\d{3})/);
    var code = statusMatch ? parseInt(statusMatch[1], 10) : 0;
    if (code === 401 || code === 403) return { icon: "\uD83D\uDD12", title: "Authentication error", detail: "Your session may have expired.", resetsAt: null };
    if (code === 429) return { icon: "", title: "Rate limited", detail: "Too many requests.", resetsAt: null };
    if (code >= 500) return { icon: "\uD83D\uDEA8", title: "Server error", detail: "The upstream provider returned an error.", resetsAt: null };
    return { icon: "\u26A0\uFE0F", title: "Error", detail: message, resetsAt: null };
  }

  function updateCountdown(el, resetsAtMs) {
    var now = Date.now();
    var diff = resetsAtMs - now;
    if (diff <= 0) {
      el.textContent = "Limit should be reset now \u2014 try again!";
      el.className = "error-countdown reset-ready";
      return true;
    }
    var hours = Math.floor(diff / 3600000);
    var mins = Math.floor((diff % 3600000) / 60000);
    var parts = [];
    if (hours > 0) parts.push(hours + "h");
    parts.push(mins + "m");
    el.textContent = "Resets in " + parts.join(" ");
    return false;
  }

  // ════════════════════════════════════════════════════════════
  // Chat page
  // ════════════════════════════════════════════════════════════
  var chatMsgBox = null;
  var chatInput = null;
  var chatSendBtn = null;

  // ── Slash commands ───────────────────────────────────────────
  var slashCommands = [
    { name: "clear", description: "Clear conversation history" },
    { name: "compact", description: "Summarize conversation to save tokens" },
    { name: "context", description: "Show session context and project info" }
  ];
  var slashMenuEl = null;
  var slashMenuIdx = 0;
  var slashMenuItems = [];

  function slashInjectStyles() {
    if (document.getElementById("slashMenuStyles")) return;
    var s = document.createElement("style");
    s.id = "slashMenuStyles";
    s.textContent =
      ".slash-menu{position:absolute;bottom:100%;left:0;right:0;background:var(--surface);border:1px solid var(--border);border-radius:var(--radius-sm);margin-bottom:4px;overflow:hidden;z-index:50;box-shadow:var(--shadow-md);animation:.1s ease-out msg-in}" +
      ".slash-menu-item{padding:7px 12px;cursor:pointer;display:flex;align-items:center;gap:8px;font-size:.8rem;color:var(--text);transition:background .1s}" +
      ".slash-menu-item:hover,.slash-menu-item.active{background:var(--bg-hover)}" +
      ".slash-menu-item .slash-name{font-weight:600;color:var(--accent);font-family:var(--font-mono);font-size:.78rem}" +
      ".slash-menu-item .slash-desc{color:var(--muted);font-size:.75rem}" +
      /* context card */
      ".ctx-card{background:var(--surface);border:1px solid var(--border);border-radius:var(--radius);align-self:center;max-width:520px;width:100%;padding:0;font-size:.8rem;line-height:1.55;animation:.2s ease-out msg-in;overflow:hidden;flex-shrink:0}" +
      ".ctx-header{background:var(--surface2);padding:10px 16px;border-bottom:1px solid var(--border);display:flex;align-items:center;gap:8px}" +
      ".ctx-header svg{flex-shrink:0;opacity:.7}" +
      ".ctx-header-title{font-weight:600;font-size:.85rem;color:var(--text)}" +
      ".ctx-section{padding:10px 16px;border-bottom:1px solid var(--border)}" +
      ".ctx-section:last-child{border-bottom:none}" +
      ".ctx-section-title{font-weight:600;font-size:.72rem;text-transform:uppercase;letter-spacing:.05em;color:var(--muted);margin-bottom:6px}" +
      ".ctx-row{display:flex;gap:8px;padding:2px 0;align-items:baseline}" +
      ".ctx-label{color:var(--muted);min-width:80px;flex-shrink:0;font-size:.78rem}" +
      ".ctx-value{color:var(--text);word-break:break-all;font-size:.78rem}" +
      ".ctx-value.mono{font-family:var(--font-mono);font-size:.74rem}" +
      ".ctx-tag{display:inline-flex;align-items:center;gap:4px;background:var(--surface2);border:1px solid var(--border);border-radius:var(--radius-sm);padding:2px 8px;font-size:.72rem;color:var(--text);margin:2px 2px 2px 0}" +
      ".ctx-tag .ctx-tag-dot{width:6px;height:6px;border-radius:50%;background:var(--accent);flex-shrink:0}" +
      ".ctx-file{font-family:var(--font-mono);font-size:.72rem;color:var(--muted);padding:3px 0;display:flex;justify-content:space-between;gap:12px}" +
      ".ctx-file-path{color:var(--text);word-break:break-all}" +
      ".ctx-file-size{flex-shrink:0;opacity:.7}" +
      ".ctx-empty{color:var(--muted);font-style:italic;font-size:.78rem;padding:2px 0}";
    document.head.appendChild(s);
  }

  function slashShowMenu(filter) {
    slashInjectStyles();
    var matches = slashCommands.filter(function (c) {
      return ("/" + c.name).indexOf(filter) === 0;
    });
    if (matches.length === 0) { slashHideMenu(); return; }
    slashMenuItems = matches;
    slashMenuIdx = 0;

    if (!slashMenuEl) {
      slashMenuEl = document.createElement("div");
      slashMenuEl.className = "slash-menu";
    }
    while (slashMenuEl.firstChild) slashMenuEl.removeChild(slashMenuEl.firstChild);
    matches.forEach(function (cmd, i) {
      var item = document.createElement("div");
      item.className = "slash-menu-item" + (i === 0 ? " active" : "");
      var nameSpan = document.createElement("span");
      nameSpan.className = "slash-name";
      nameSpan.textContent = "/" + cmd.name;
      var descSpan = document.createElement("span");
      descSpan.className = "slash-desc";
      descSpan.textContent = cmd.description;
      item.appendChild(nameSpan);
      item.appendChild(descSpan);
      item.addEventListener("mousedown", function (e) {
        e.preventDefault();
        slashSelectItem(i);
      });
      slashMenuEl.appendChild(item);
    });

    var inputWrap = chatInput.parentElement;
    if (inputWrap && !slashMenuEl.parentElement) {
      inputWrap.style.position = "relative";
      inputWrap.appendChild(slashMenuEl);
    }
  }

  function slashHideMenu() {
    if (slashMenuEl && slashMenuEl.parentElement) {
      slashMenuEl.parentElement.removeChild(slashMenuEl);
    }
    slashMenuItems = [];
    slashMenuIdx = 0;
  }

  function slashSelectItem(idx) {
    if (!slashMenuItems[idx]) return;
    chatInput.value = "/" + slashMenuItems[idx].name;
    slashHideMenu();
    sendChat();
  }

  function slashHandleInput() {
    var val = chatInput.value;
    if (val.indexOf("/") === 0 && val.indexOf(" ") === -1) {
      slashShowMenu(val);
    } else {
      slashHideMenu();
    }
  }

  function slashHandleKeydown(e) {
    if (!slashMenuEl || !slashMenuEl.parentElement || slashMenuItems.length === 0) return false;
    if (e.key === "ArrowUp") {
      e.preventDefault();
      slashMenuIdx = (slashMenuIdx - 1 + slashMenuItems.length) % slashMenuItems.length;
      slashUpdateActive();
      return true;
    }
    if (e.key === "ArrowDown") {
      e.preventDefault();
      slashMenuIdx = (slashMenuIdx + 1) % slashMenuItems.length;
      slashUpdateActive();
      return true;
    }
    if (e.key === "Enter" || e.key === "Tab") {
      e.preventDefault();
      slashSelectItem(slashMenuIdx);
      return true;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      slashHideMenu();
      return true;
    }
    return false;
  }

  function slashUpdateActive() {
    if (!slashMenuEl) return;
    var items = slashMenuEl.querySelectorAll(".slash-menu-item");
    items.forEach(function (el, i) {
      el.classList.toggle("active", i === slashMenuIdx);
    });
  }

  function ctxEl(tag, cls, text) {
    var el = document.createElement(tag);
    if (cls) el.className = cls;
    if (text !== undefined) el.textContent = text;
    return el;
  }

  function ctxRow(label, value, mono) {
    var row = ctxEl("div", "ctx-row");
    row.appendChild(ctxEl("span", "ctx-label", label));
    row.appendChild(ctxEl("span", "ctx-value" + (mono ? " mono" : ""), value));
    return row;
  }

  function ctxSection(title) {
    var sec = ctxEl("div", "ctx-section");
    sec.appendChild(ctxEl("div", "ctx-section-title", title));
    return sec;
  }

  function renderContextCard(data) {
    if (!chatMsgBox) return;
    slashInjectStyles();

    var card = ctxEl("div", "ctx-card");

    // Header with icon
    var header = ctxEl("div", "ctx-header");
    var svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("width", "16");
    svg.setAttribute("height", "16");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "2");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");
    var path1 = document.createElementNS("http://www.w3.org/2000/svg", "circle");
    path1.setAttribute("cx", "12");
    path1.setAttribute("cy", "12");
    path1.setAttribute("r", "3");
    var path2 = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path2.setAttribute("d", "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z");
    svg.appendChild(path1);
    svg.appendChild(path2);
    header.appendChild(svg);
    header.appendChild(ctxEl("span", "ctx-header-title", "Context"));
    card.appendChild(header);

    // Session section
    var sess = data.session || {};
    var sessSection = ctxSection("Session");
    sessSection.appendChild(ctxRow("Key", sess.key || "unknown", true));
    sessSection.appendChild(ctxRow("Messages", String(sess.messageCount || 0)));
    sessSection.appendChild(ctxRow("Model", sess.model || "default", true));
    if (sess.label) sessSection.appendChild(ctxRow("Label", sess.label));
    card.appendChild(sessSection);

    // Project section
    var proj = data.project;
    var projSection = ctxSection("Project");
    if (proj && proj !== null) {
      projSection.appendChild(ctxRow("Name", proj.label || "(unnamed)"));
      if (proj.directory) projSection.appendChild(ctxRow("Directory", proj.directory, true));
      if (proj.systemPrompt) projSection.appendChild(ctxRow("System Prompt", proj.systemPrompt.length + " chars"));

      var ctxFiles = proj.contextFiles || [];
      if (ctxFiles.length > 0) {
        var filesLabel = ctxEl("div", "ctx-section-title", "Context Files (" + ctxFiles.length + ")");
        filesLabel.style.marginTop = "8px";
        projSection.appendChild(filesLabel);
        ctxFiles.forEach(function (f) {
          var row = ctxEl("div", "ctx-file");
          row.appendChild(ctxEl("span", "ctx-file-path", f.path));
          row.appendChild(ctxEl("span", "ctx-file-size", formatBytes(f.size)));
          projSection.appendChild(row);
        });
      }
    } else {
      projSection.appendChild(ctxEl("div", "ctx-empty", "No project bound to this session"));
    }
    card.appendChild(projSection);

    // Tools section
    var tools = data.tools || [];
    var toolsSection = ctxSection("Tools");
    if (tools.length > 0) {
      var toolWrap = ctxEl("div", "");
      toolWrap.style.cssText = "display:flex;flex-wrap:wrap;gap:0";
      tools.forEach(function (t) {
        var tag = ctxEl("span", "ctx-tag");
        var dot = ctxEl("span", "ctx-tag-dot");
        tag.appendChild(dot);
        tag.appendChild(document.createTextNode(t.name));
        tag.title = t.description;
        toolWrap.appendChild(tag);
      });
      toolsSection.appendChild(toolWrap);
    } else {
      toolsSection.appendChild(ctxEl("div", "ctx-empty", "No tools registered"));
    }
    card.appendChild(toolsSection);

    // Sandbox section
    var sb = data.sandbox || {};
    var sandboxSection = ctxSection("Sandbox");
    sandboxSection.appendChild(ctxRow("Enabled", sb.enabled ? "yes" : "no", true));
    if (sb.backend) {
      sandboxSection.appendChild(ctxRow("Backend", sb.backend));
      if (sb.mode) sandboxSection.appendChild(ctxRow("Mode", sb.mode));
      if (sb.scope) sandboxSection.appendChild(ctxRow("Scope", sb.scope));
      if (sb.workspaceMount) sandboxSection.appendChild(ctxRow("Workspace Mount", sb.workspaceMount));
      if (sb.image) sandboxSection.appendChild(ctxRow("Image", sb.image, true));
    }
    card.appendChild(sandboxSection);

    // Token Usage section
    var tu = data.tokenUsage || {};
    var tokenSection = ctxSection("Token Usage (estimated)");
    tokenSection.appendChild(ctxRow("Conversation", formatTokens(tu.conversationTokens || 0)));
    if (tu.contextFileTokens > 0) {
      tokenSection.appendChild(ctxRow("Context Files", formatTokens(tu.contextFileTokens)));
    }
    if (tu.systemPromptTokens > 0) {
      tokenSection.appendChild(ctxRow("System Prompt", formatTokens(tu.systemPromptTokens)));
    }
    tokenSection.appendChild(ctxRow("Total", formatTokens(tu.estimatedTotal || 0), true));
    if (tu.contextWindow > 0) {
      tokenSection.appendChild(ctxRow("Context Window", formatTokens(tu.contextWindow)));
      sessionContextWindow = tu.contextWindow;
      updateTokenBar();
    }
    card.appendChild(tokenSection);

    chatMsgBox.appendChild(card);
    chatMsgBox.scrollTop = chatMsgBox.scrollHeight;
  }

  function renderCompactCard(data) {
    if (!chatMsgBox) return;
    slashInjectStyles();

    var card = ctxEl("div", "ctx-card");

    // Header with compress icon
    var header = ctxEl("div", "ctx-header");
    var svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("width", "16");
    svg.setAttribute("height", "16");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "2");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");
    // Compress/shrink icon (two arrows pointing inward)
    var p1 = document.createElementNS("http://www.w3.org/2000/svg", "polyline");
    p1.setAttribute("points", "4 14 10 14 10 20");
    var p2 = document.createElementNS("http://www.w3.org/2000/svg", "polyline");
    p2.setAttribute("points", "20 10 14 10 14 4");
    var l1 = document.createElementNS("http://www.w3.org/2000/svg", "line");
    l1.setAttribute("x1", "14"); l1.setAttribute("y1", "10");
    l1.setAttribute("x2", "21"); l1.setAttribute("y2", "3");
    var l2 = document.createElementNS("http://www.w3.org/2000/svg", "line");
    l2.setAttribute("x1", "3"); l2.setAttribute("y1", "21");
    l2.setAttribute("x2", "10"); l2.setAttribute("y2", "14");
    svg.appendChild(p1);
    svg.appendChild(p2);
    svg.appendChild(l1);
    svg.appendChild(l2);
    header.appendChild(svg);
    header.appendChild(ctxEl("span", "ctx-header-title", "Conversation compacted"));
    card.appendChild(header);

    // Stats section
    var statsSection = ctxSection("Before compact");
    statsSection.appendChild(ctxRow("Messages", String(data.messageCount || 0)));
    statsSection.appendChild(ctxRow("Total tokens", formatTokens(data.totalTokens || 0)));
    if (data.contextWindow) {
      var pctUsed = Math.round((data.totalTokens || 0) / data.contextWindow * 100);
      statsSection.appendChild(ctxRow("Context usage", pctUsed + "% of " + formatTokens(data.contextWindow)));
    }
    card.appendChild(statsSection);

    // After section
    var afterSection = ctxSection("After compact");
    afterSection.appendChild(ctxRow("Messages", "1 (summary)"));
    afterSection.appendChild(ctxRow("Status", "Conversation history replaced with a summary"));
    card.appendChild(afterSection);

    chatMsgBox.appendChild(card);
    chatMsgBox.scrollTop = chatMsgBox.scrollHeight;
  }

  function formatBytes(b) {
    if (b >= 1024) return (b / 1024).toFixed(1) + " KB";
    return b + " B";
  }

  function formatTokens(n) {
    if (n >= 1000000) return (n / 1000000).toFixed(1) + "M";
    if (n >= 1000) return (n / 1000).toFixed(1) + "k";
    return String(n);
  }

  var chatBatchLoading = false;

  // Scroll chat to bottom and keep it pinned until layout settles.
  // Uses a ResizeObserver to catch any late layout shifts (sidebar re-render,
  // font loading, async style recalc) and re-scrolls until stable.
  function scrollChatToBottom() {
    if (!chatMsgBox) return;
    chatMsgBox.scrollTop = chatMsgBox.scrollHeight;
    var box = chatMsgBox;
    var observer = new ResizeObserver(function () {
      box.scrollTop = box.scrollHeight;
    });
    observer.observe(box);
    setTimeout(function () { observer.disconnect(); }, 500);
  }

  function chatAddMsg(cls, content, isHtml) {
    if (!chatMsgBox) return null;
    var el = document.createElement("div");
    el.className = "msg " + cls;
    if (isHtml) {
      // Safe: content is produced by renderMarkdown which escapes via esc() first,
      // then only adds our own formatting tags (pre, code, strong).
      el.innerHTML = content;
    } else {
      el.textContent = content;
    }
    chatMsgBox.appendChild(el);
    if (!chatBatchLoading) chatMsgBox.scrollTop = chatMsgBox.scrollHeight;
    return el;
  }

  function stripChannelPrefix(text) {
    return text.replace(/^\[Telegram(?:\s+from\s+[^\]]+)?\]\s*/, "");
  }

  function appendChannelFooter(el, channel) {
    var ft = document.createElement("div");
    ft.className = "msg-channel-footer";
    var label = channel.channel_type || "channel";
    var who = channel.username ? "@" + channel.username : channel.sender_name;
    if (who) label += " \u00b7 " + who;
    ft.textContent = "via " + label;
    el.appendChild(ft);
  }

  function removeThinking() {
    var el = document.getElementById("thinkingIndicator");
    if (el) el.remove();
  }

  function chatAddErrorCard(err) {
    if (!chatMsgBox) return;
    var el = document.createElement("div");
    el.className = "msg error-card";

    var icon = document.createElement("div");
    icon.className = "error-icon";
    icon.textContent = err.icon || "\u26A0\uFE0F";
    el.appendChild(icon);

    var body = document.createElement("div");
    body.className = "error-body";

    var title = document.createElement("div");
    title.className = "error-title";
    title.textContent = err.title;
    body.appendChild(title);

    if (err.detail) {
      var detail = document.createElement("div");
      detail.className = "error-detail";
      detail.textContent = err.detail;
      body.appendChild(detail);
    }

    if (err.provider) {
      var prov = document.createElement("div");
      prov.className = "error-detail";
      prov.textContent = "Provider: " + err.provider;
      prov.style.marginTop = "4px";
      prov.style.opacity = "0.6";
      body.appendChild(prov);
    }

    if (err.resetsAt) {
      var countdown = document.createElement("div");
      countdown.className = "error-countdown";
      el.appendChild(body);
      el.appendChild(countdown);
      updateCountdown(countdown, err.resetsAt);
      var timer = setInterval(function () {
        if (updateCountdown(countdown, err.resetsAt)) clearInterval(timer);
      }, 1000);
    } else {
      el.appendChild(body);
    }

    chatMsgBox.appendChild(el);
    chatMsgBox.scrollTop = chatMsgBox.scrollHeight;
  }

  function chatAddErrorMsg(message) {
    chatAddErrorCard(parseErrorMessage(message));
  }

  function renderApprovalCard(requestId, command) {
    if (!chatMsgBox) return;
    var card = document.createElement("div");
    card.className = "msg approval-card";
    card.id = "approval-" + requestId;

    var label = document.createElement("div");
    label.className = "approval-label";
    label.textContent = "Command requires approval:";
    card.appendChild(label);

    var cmdEl = document.createElement("code");
    cmdEl.className = "approval-cmd";
    cmdEl.textContent = command;
    card.appendChild(cmdEl);

    var btnGroup = document.createElement("div");
    btnGroup.className = "approval-btns";

    var allowBtn = document.createElement("button");
    allowBtn.className = "approval-btn approval-allow";
    allowBtn.textContent = "Allow";
    allowBtn.onclick = function () { resolveApproval(requestId, "approved", command, card); };

    var denyBtn = document.createElement("button");
    denyBtn.className = "approval-btn approval-deny";
    denyBtn.textContent = "Deny";
    denyBtn.onclick = function () { resolveApproval(requestId, "denied", null, card); };

    btnGroup.appendChild(allowBtn);
    btnGroup.appendChild(denyBtn);
    card.appendChild(btnGroup);

    var countdown = document.createElement("div");
    countdown.className = "approval-countdown";
    card.appendChild(countdown);
    var remaining = 120;
    var timer = setInterval(function () {
      remaining--;
      countdown.textContent = remaining + "s";
      if (remaining <= 0) {
        clearInterval(timer);
        card.classList.add("approval-expired");
        allowBtn.disabled = true;
        denyBtn.disabled = true;
        countdown.textContent = "expired";
      }
    }, 1000);
    countdown.textContent = remaining + "s";

    chatMsgBox.appendChild(card);
    chatMsgBox.scrollTop = chatMsgBox.scrollHeight;
  }

  function resolveApproval(requestId, decision, command, card) {
    var params = { requestId: requestId, decision: decision };
    if (command) params.command = command;
    sendRpc("exec.approval.resolve", params).then(function () {
      card.classList.add("approval-resolved");
      card.querySelectorAll(".approval-btn").forEach(function (b) { b.disabled = true; });
      var status = document.createElement("div");
      status.className = "approval-status";
      status.textContent = decision === "approved" ? "Allowed" : "Denied";
      card.appendChild(status);
    });
  }

  function switchSession(key, searchContext, projectId) {
    activeSessionKey = key;
    localStorage.setItem("moltis-session", key);
    if (chatMsgBox) chatMsgBox.textContent = "";
    streamEl = null;
    streamText = "";
    sessionTokens = { input: 0, output: 0 };
    sessionContextWindow = 0;
    updateTokenBar();

    var items = sessionList.querySelectorAll(".session-item");
    items.forEach(function (el) {
      var isTarget = el.getAttribute("data-session-key") === key;
      el.classList.toggle("active", isTarget);
      if (isTarget) el.classList.remove("unread");
    });

    var switchParams = { key: key };
    if (projectId) switchParams.project_id = projectId;
    sendRpc("sessions.switch", switchParams).then(function (res) {
      if (res && res.ok && res.payload) {
        var entry = res.payload.entry || {};
        // Restore the session's project binding.
        // If we explicitly passed a projectId (e.g. new session), keep it
        // even if the server response hasn't persisted it yet.
        var effectiveProjectId = entry.projectId || projectId || "";
        activeProjectId = effectiveProjectId;
        localStorage.setItem("moltis-project", activeProjectId);
        updateSessionProjectSelect(activeProjectId);
        // Restore per-session model
        if (entry.model && models.length > 0) {
          var found = models.find(function (m) { return m.id === entry.model; });
          if (found) {
            selectedModelId = found.id;
            if (modelComboLabel) modelComboLabel.textContent = found.displayName || found.id;
            localStorage.setItem("moltis-model", found.id);
          }
        }
        // Restore sandbox state
        updateSandboxUI(entry.sandbox_enabled !== false);
        var history = res.payload.history || [];
        var msgEls = [];
        sessionTokens = { input: 0, output: 0 };
        chatBatchLoading = true;
        history.forEach(function (msg) {
          if (msg.role === "user") {
            var userContent = msg.content || "";
            if (msg.channel) userContent = stripChannelPrefix(userContent);
            var userEl = chatAddMsg("user", renderMarkdown(userContent), true);
            if (userEl && msg.channel) appendChannelFooter(userEl, msg.channel);
            msgEls.push(userEl);
          } else if (msg.role === "assistant") {
            var el = chatAddMsg("assistant", renderMarkdown(msg.content || ""), true);
            if (el && msg.model) {
              var ft = document.createElement("div");
              ft.className = "msg-model-footer";
              var ftText = msg.provider ? msg.provider + " / " + msg.model : msg.model;
              if (msg.inputTokens || msg.outputTokens) {
                ftText += " \u00b7 " + formatTokens(msg.inputTokens || 0) + " in / " + formatTokens(msg.outputTokens || 0) + " out";
              }
              ft.textContent = ftText;
              el.appendChild(ft);
            }
            if (msg.inputTokens || msg.outputTokens) {
              sessionTokens.input += (msg.inputTokens || 0);
              sessionTokens.output += (msg.outputTokens || 0);
            }
            msgEls.push(el);
          } else {
            msgEls.push(null);
          }
        });
        chatBatchLoading = false;
        // Fetch context window for the token bar percentage display.
        sendRpc("chat.context", {}).then(function (ctxRes) {
          if (ctxRes && ctxRes.ok && ctxRes.payload && ctxRes.payload.tokenUsage) {
            sessionContextWindow = ctxRes.payload.tokenUsage.contextWindow || 0;
          }
          updateTokenBar();
        });
        updateTokenBar();

        if (searchContext && searchContext.query && chatMsgBox) {
          highlightAndScroll(msgEls, searchContext.messageIndex, searchContext.query);
        } else {
          scrollChatToBottom();
        }

        var item = sessionList.querySelector('.session-item[data-session-key="' + key + '"]');
        if (item && item.classList.contains("replying") && chatMsgBox) {
          removeThinking();
          var thinkEl = document.createElement("div");
          thinkEl.className = "msg assistant thinking";
          thinkEl.id = "thinkingIndicator";
          var thinkDots = document.createElement("span");
          thinkDots.className = "thinking-dots";
          // Safe: static hardcoded HTML, no user input
          thinkDots.innerHTML = "<span></span><span></span><span></span>";
          thinkEl.appendChild(thinkDots);
          chatMsgBox.appendChild(thinkEl);
          scrollChatToBottom();
        }
        if (!sessionList.querySelector('.session-meta[data-session-key="' + key + '"]')) {
          fetchSessions();
        }
      }
    });
  }

  function highlightAndScroll(msgEls, messageIndex, query) {
    var target = null;
    if (messageIndex >= 0 && messageIndex < msgEls.length && msgEls[messageIndex]) {
      target = msgEls[messageIndex];
    }
    var lowerQ = query.toLowerCase();
    if (!target || (target.textContent || "").toLowerCase().indexOf(lowerQ) === -1) {
      for (var i = 0; i < msgEls.length; i++) {
        if (msgEls[i] && (msgEls[i].textContent || "").toLowerCase().indexOf(lowerQ) !== -1) {
          target = msgEls[i];
          break;
        }
      }
    }
    if (!target) return;
    msgEls.forEach(function (el) { if (el) highlightTermInElement(el, query); });
    target.scrollIntoView({ behavior: "smooth", block: "center" });
    target.classList.add("search-highlight-msg");
    setTimeout(function () {
      if (!chatMsgBox) return;
      chatMsgBox.querySelectorAll("mark.search-term-highlight").forEach(function (m) {
        var parent = m.parentNode;
        parent.replaceChild(document.createTextNode(m.textContent), m);
        parent.normalize();
      });
      chatMsgBox.querySelectorAll(".search-highlight-msg").forEach(function (el) {
        el.classList.remove("search-highlight-msg");
      });
    }, 5000);
  }

  function highlightTermInElement(el, query) {
    var walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT, null, false);
    var nodes = [];
    while (walker.nextNode()) nodes.push(walker.currentNode);
    var lowerQ = query.toLowerCase();
    nodes.forEach(function (textNode) {
      var text = textNode.nodeValue;
      var lowerText = text.toLowerCase();
      var idx = lowerText.indexOf(lowerQ);
      if (idx === -1) return;
      var frag = document.createDocumentFragment();
      var pos = 0;
      while (idx !== -1) {
        if (idx > pos) frag.appendChild(document.createTextNode(text.substring(pos, idx)));
        var mark = document.createElement("mark");
        mark.className = "search-term-highlight";
        mark.textContent = text.substring(idx, idx + query.length);
        frag.appendChild(mark);
        pos = idx + query.length;
        idx = lowerText.indexOf(lowerQ, pos);
      }
      if (pos < text.length) frag.appendChild(document.createTextNode(text.substring(pos)));
      textNode.parentNode.replaceChild(frag, textNode);
    });
  }

  function sendChat() {
    var text = chatInput.value.trim();
    if (!text || !connected) return;

    // Slash command dispatch
    if (text.charAt(0) === "/") {
      var cmdName = text.substring(1).toLowerCase();
      var matched = slashCommands.find(function (c) { return c.name === cmdName; });
      if (matched) {
        chatInput.value = "";
        chatAutoResize();
        slashHideMenu();
        if (cmdName === "clear") {
          sendRpc("chat.clear", {}).then(function (res) {
            if (res && res.ok) {
              if (chatMsgBox) chatMsgBox.textContent = "";
              sessionTokens = { input: 0, output: 0 };
              updateTokenBar();
              var metaEl = sessionList.querySelector('.session-meta[data-session-key="' + activeSessionKey + '"]');
              if (metaEl) metaEl.textContent = "0 msgs";
            } else {
              chatAddMsg("error", (res && res.error && res.error.message) || "Clear failed");
            }
          });
        } else if (cmdName === "compact") {
          chatAddMsg("system", "Compacting conversation\u2026");
          sendRpc("chat.compact", {}).then(function (res) {
            if (res && res.ok) {
              switchSession(activeSessionKey);
            } else {
              chatAddMsg("error", (res && res.error && res.error.message) || "Compact failed");
            }
          });
        } else if (cmdName === "context") {
          chatAddMsg("system", "Loading context\u2026");
          sendRpc("chat.context", {}).then(function (res) {
            // Remove the "Loading context..." message
            if (chatMsgBox && chatMsgBox.lastChild) chatMsgBox.removeChild(chatMsgBox.lastChild);
            if (res && res.ok && res.payload) {
              try { renderContextCard(res.payload); }
              catch (err) { chatAddMsg("error", "Render error: " + err.message); }
            } else {
              chatAddMsg("error", (res && res.error && res.error.message) || "Context failed");
            }
          });
        }
        return;
      }
    }

    chatHistory.push(text);
    if (chatHistory.length > 200) chatHistory = chatHistory.slice(-200);
    localStorage.setItem("moltis-chat-history", JSON.stringify(chatHistory));
    chatHistoryIdx = -1;
    chatHistoryDraft = "";
    chatInput.value = "";
    chatAutoResize();
    chatAddMsg("user", renderMarkdown(text), true);
    var chatParams = { text: text };
    var selectedModel = selectedModelId;
    if (selectedModel) {
      chatParams.model = selectedModel;
      setSessionModel(activeSessionKey, selectedModel);
    }
    bumpSessionCount(activeSessionKey, 1);
    setSessionReplying(activeSessionKey, true);
    sendRpc("chat.send", chatParams).then(function (res) {
      if (res && !res.ok && res.error) {
        chatAddMsg("error", res.error.message || "Request failed");
      }
    });
  }

  function chatAutoResize() {
    if (!chatInput) return;
    chatInput.style.height = "auto";
    chatInput.style.height = Math.min(chatInput.scrollHeight, 120) + "px";
  }

  function formatTokens(n) {
    if (n >= 1000000) return (n / 1000000).toFixed(1) + "M";
    if (n >= 1000) return (n / 1000).toFixed(1) + "K";
    return String(n);
  }

  var sessionContextWindow = 0;

  function updateTokenBar() {
    var bar = $("tokenBar");
    if (!bar) return;
    var total = sessionTokens.input + sessionTokens.output;
    if (total === 0) {
      bar.textContent = "";
      return;
    }
    var text =
      formatTokens(sessionTokens.input) + " in / " +
      formatTokens(sessionTokens.output) + " out \u00b7 " +
      formatTokens(total) + " tokens";
    if (sessionContextWindow > 0) {
      var pct = Math.max(0, 100 - Math.round(total / sessionContextWindow * 100));
      text += " \u00b7 Context left before auto-compact: " + pct + "%";
    }
    bar.textContent = text;
  }

  // Safe: static hardcoded HTML template, no user input.
  var chatPageHTML =
    '<div class="flex-1 flex flex-col min-w-0">' +
      '<div class="px-4 py-1.5 border-b border-[var(--border)] bg-[var(--surface)] flex items-center gap-2 shrink-0">' +
        '<div id="projectCombo" class="model-combo">' +
          '<button id="projectComboBtn" class="model-combo-btn" type="button">' +
            '<span id="projectComboLabel">No project</span>' +
            '<svg class="model-combo-chevron" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="2" stroke="currentColor" width="12" height="12"><path d="M19.5 8.25l-7.5 7.5-7.5-7.5"/></svg>' +
          '</button>' +
          '<div id="projectDropdown" class="model-dropdown hidden">' +
            '<div id="projectDropdownList" class="model-dropdown-list"></div>' +
          '</div>' +
        '</div>' +
        '<div id="modelCombo" class="model-combo">' +
          '<button id="modelComboBtn" class="model-combo-btn" type="button">' +
            '<span id="modelComboLabel">' + (selectedModelId || 'loading\u2026') + '</span>' +
            '<svg class="model-combo-chevron" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="2" stroke="currentColor" width="12" height="12"><path d="M19.5 8.25l-7.5 7.5-7.5-7.5"/></svg>' +
          '</button>' +
          '<div id="modelDropdown" class="model-dropdown hidden">' +
            '<input id="modelSearchInput" type="text" placeholder="Search models\u2026" class="model-search-input" autocomplete="off" />' +
            '<div id="modelDropdownList" class="model-dropdown-list"></div>' +
          '</div>' +
        '</div>' +
        '<button id="sandboxToggle" class="sandbox-toggle text-xs border border-[var(--border)] px-2 py-1 rounded-md transition-colors cursor-pointer bg-transparent font-[var(--font-body)]" title="Toggle sandbox mode">' +
          '<svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" width="14" height="14" style="display:inline-block;vertical-align:middle;margin-right:2px"><path d="M16.5 10.5V6.75a4.5 4.5 0 1 0-9 0v3.75m-.75 11.25h10.5a2.25 2.25 0 0 0 2.25-2.25v-6.75a2.25 2.25 0 0 0-2.25-2.25H6.75a2.25 2.25 0 0 0-2.25 2.25v6.75a2.25 2.25 0 0 0 2.25 2.25Z"/></svg>' +
          '<span id="sandboxLabel">sandboxed</span>' +
        '</button>' +
      '</div>' +
      '<div class="flex-1 overflow-y-auto p-4 flex flex-col gap-2" id="messages"></div>' +
      '<div id="tokenBar" class="token-bar"></div>' +
      '<div class="px-4 py-3 border-t border-[var(--border)] bg-[var(--surface)] flex gap-2 items-end">' +
        '<textarea id="chatInput" placeholder="Type a message..." rows="1" ' +
          'class="flex-1 bg-[var(--surface2)] border border-[var(--border)] text-[var(--text)] px-3 py-2 rounded-lg text-sm resize-none min-h-[40px] max-h-[120px] leading-relaxed focus:outline-none focus:border-[var(--border-strong)] focus:ring-1 focus:ring-[var(--accent-subtle)] transition-colors font-[var(--font-body)]"></textarea>' +
        '<button id="sendBtn" disabled ' +
          'class="bg-[var(--accent-dim)] text-white border-none px-4 py-2 rounded-lg cursor-pointer text-sm font-medium whitespace-nowrap hover:bg-[var(--accent)] disabled:opacity-40 disabled:cursor-default transition-colors">Send</button>' +
      '</div></div>';

  registerPage("/", function initChat(container) {
    container.innerHTML = chatPageHTML;

    chatMsgBox = $("messages");
    chatInput = $("chatInput");
    chatSendBtn = $("sendBtn");

    // Bind model selector elements (now inside chat page)
    modelCombo = $("modelCombo");
    modelComboBtn = $("modelComboBtn");
    modelComboLabel = $("modelComboLabel");
    modelDropdown = $("modelDropdown");
    modelSearchInput = $("modelSearchInput");
    modelDropdownList = $("modelDropdownList");
    bindModelComboEvents();

    // Bind sandbox toggle elements (now inside chat page)
    sandboxToggleBtn = $("sandboxToggle");
    sandboxLabel = $("sandboxLabel");
    bindSandboxToggleEvents();
    updateSandboxUI(true); // default: sandboxed until session loads

    // Bind session project combo
    projectCombo = $("projectCombo");
    projectComboBtn = $("projectComboBtn");
    projectComboLabel = $("projectComboLabel");
    projectDropdown = $("projectDropdown");
    projectDropdownList = $("projectDropdownList");
    bindProjectComboEvents();

    // Update model selector label if models are already loaded
    if (models.length > 0 && modelComboLabel) {
      var found = models.find(function (m) { return m.id === selectedModelId; });
      if (found) {
        modelComboLabel.textContent = found.displayName || found.id;
      } else if (models[0]) {
        modelComboLabel.textContent = models[0].displayName || models[0].id;
      }
    }

    if (connected) {
      chatSendBtn.disabled = false;
      switchSession(activeSessionKey);
    }

    chatInput.addEventListener("input", function () { chatAutoResize(); slashHandleInput(); });
    chatInput.addEventListener("keydown", function (e) {
      if (slashHandleKeydown(e)) return;
      if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendChat(); return; }
      if (e.key === "ArrowUp" && chatInput.selectionStart === 0 && !e.shiftKey) {
        if (chatHistory.length === 0) return;
        e.preventDefault();
        if (chatHistoryIdx === -1) {
          chatHistoryDraft = chatInput.value;
          chatHistoryIdx = chatHistory.length - 1;
        } else if (chatHistoryIdx > 0) {
          chatHistoryIdx--;
        }
        chatInput.value = chatHistory[chatHistoryIdx];
        chatAutoResize();
        return;
      }
      if (e.key === "ArrowDown" && chatInput.selectionStart === chatInput.value.length && !e.shiftKey) {
        if (chatHistoryIdx === -1) return;
        e.preventDefault();
        if (chatHistoryIdx < chatHistory.length - 1) {
          chatHistoryIdx++;
          chatInput.value = chatHistory[chatHistoryIdx];
        } else {
          chatHistoryIdx = -1;
          chatInput.value = chatHistoryDraft;
        }
        chatAutoResize();
        return;
      }
    });
    chatSendBtn.addEventListener("click", sendChat);

    if (connected) switchSession(activeSessionKey);
    chatInput.focus();
  }, function teardownChat() {
    slashHideMenu();
    chatMsgBox = null;
    chatInput = null;
    chatSendBtn = null;
    streamEl = null;
    streamText = "";
    modelCombo = null;
    modelComboBtn = null;
    modelComboLabel = null;
    modelDropdown = null;
    modelSearchInput = null;
    modelDropdownList = null;
    sandboxToggleBtn = null;
    sandboxLabel = null;
    projectCombo = null;
    projectComboBtn = null;
    projectComboLabel = null;
    projectDropdown = null;
    projectDropdownList = null;
  });

  // ════════════════════════════════════════════════════════════
  // Methods page
  // ════════════════════════════════════════════════════════════
  // Safe: static hardcoded HTML template, no user input.
  var methodsPageHTML =
    '<div class="flex-1 flex flex-col min-w-0 p-4 gap-3">' +
      '<h2 class="text-lg font-medium text-[var(--text-strong)]">Method Explorer</h2>' +
      '<div><label class="text-xs text-[var(--muted)] block mb-1">Method</label>' +
        '<input id="rpcMethod" placeholder="e.g. health" value="health" class="w-full bg-[var(--surface2)] border border-[var(--border)] text-[var(--text)] px-2 py-1.5 rounded text-xs font-[var(--font-mono)] focus:outline-none focus:border-[var(--border-strong)]" style="max-width:400px"></div>' +
      '<div><label class="text-xs text-[var(--muted)] block mb-1">Params (JSON, optional)</label>' +
        '<textarea id="rpcParams" placeholder="{}" class="w-full bg-[var(--surface2)] border border-[var(--border)] text-[var(--text)] px-2 py-1.5 rounded text-xs font-[var(--font-mono)] min-h-[80px] resize-y focus:outline-none focus:border-[var(--border-strong)]" style="max-width:400px"></textarea></div>' +
      '<button id="rpcSend" class="bg-[var(--accent-dim)] text-white border-none px-3 py-1.5 rounded text-xs cursor-pointer hover:bg-[var(--accent)] transition-colors self-start">Call</button>' +
      '<div><label class="text-xs text-[var(--muted)] block mb-1">Response</label>' +
        '<div class="methods-result" id="rpcResult"></div></div></div>';

  registerPage("/methods", function initMethods(container) {
    container.innerHTML = methodsPageHTML;

    var rpcMethod = $("rpcMethod");
    var rpcParams = $("rpcParams");
    var rpcSend = $("rpcSend");
    var rpcResult = $("rpcResult");

    rpcSend.addEventListener("click", function () {
      var method = rpcMethod.value.trim();
      if (!method || !connected) return;
      var params;
      var raw = rpcParams.value.trim();
      if (raw) {
        try { params = JSON.parse(raw); } catch (e) {
          rpcResult.textContent = "Invalid JSON: " + e.message;
          return;
        }
      }
      rpcResult.textContent = "calling...";
      sendRpc(method, params).then(function (res) {
        rpcResult.textContent = JSON.stringify(res, null, 2);
      });
    });
  });

  // ════════════════════════════════════════════════════════════
  // Crons page
  // ════════════════════════════════════════════════════════════
  // Safe: static hardcoded HTML template, no user input.
  var cronsPageHTML =
    '<div class="flex-1 flex flex-col min-w-0 p-4 gap-4 overflow-y-auto">' +
      '<div class="flex items-center gap-3">' +
        '<h2 class="text-lg font-medium text-[var(--text-strong)]">Cron Jobs</h2>' +
        '<button id="cronAddBtn" class="bg-[var(--accent-dim)] text-white border-none px-3 py-1.5 rounded text-xs cursor-pointer hover:bg-[var(--accent)] transition-colors">+ Add Job</button>' +
        '<button id="cronRefreshBtn" class="text-xs text-[var(--muted)] border border-[var(--border)] px-2.5 py-1 rounded-md hover:text-[var(--text)] hover:border-[var(--border-strong)] transition-colors cursor-pointer bg-transparent">Refresh</button>' +
      '</div>' +
      '<div id="cronStatusBar" class="cron-status-bar"></div>' +
      '<div id="cronJobList"></div>' +
      '<div id="cronRunsPanel" class="hidden"></div>' +
    '</div>';

  registerPage("/crons", function initCrons(container) {
    container.innerHTML = cronsPageHTML;

    var cronStatusBar = $("cronStatusBar");
    var cronJobList = $("cronJobList");
    var cronRunsPanel = $("cronRunsPanel");

    function loadStatus() {
      sendRpc("cron.status", {}).then(function (res) {
        if (!res || !res.ok) { cronStatusBar.textContent = "Failed to load status"; return; }
        var s = res.payload;
        var parts = [
          s.running ? "Running" : "Stopped",
          s.jobCount + " job" + (s.jobCount !== 1 ? "s" : ""),
          s.enabledCount + " enabled"
        ];
        if (s.nextRunAtMs) {
          parts.push("next: " + new Date(s.nextRunAtMs).toLocaleString());
        }
        cronStatusBar.textContent = parts.join(" \u2022 ");
      });
    }

    function loadJobs() {
      sendRpc("cron.list", {}).then(function (res) {
        if (!res || !res.ok) { cronJobList.textContent = "Failed to load jobs"; return; }
        renderJobTable(res.payload || []);
      });
    }

    function renderJobTable(jobs) {
      cronJobList.textContent = "";
      if (jobs.length === 0) {
        var empty = document.createElement("div");
        empty.className = "text-sm text-[var(--muted)]";
        empty.textContent = "No cron jobs configured.";
        cronJobList.appendChild(empty);
        return;
      }
      var table = document.createElement("table");
      table.className = "cron-table";

      var thead = document.createElement("thead");
      var headRow = document.createElement("tr");
      ["Name", "Schedule", "Next Run", "Last Status", "Enabled", "Actions"].forEach(function (h) {
        var th = document.createElement("th");
        th.textContent = h;
        headRow.appendChild(th);
      });
      thead.appendChild(headRow);
      table.appendChild(thead);

      var tbody = document.createElement("tbody");
      jobs.forEach(function (job) {
        var tr = document.createElement("tr");

        var tdName = document.createElement("td");
        tdName.textContent = job.name;
        tr.appendChild(tdName);

        var tdSched = document.createElement("td");
        tdSched.textContent = formatSchedule(job.schedule);
        tdSched.style.fontFamily = "var(--font-mono)";
        tdSched.style.fontSize = ".78rem";
        tr.appendChild(tdSched);

        var tdNext = document.createElement("td");
        tdNext.style.fontSize = ".78rem";
        tdNext.textContent = job.state && job.state.nextRunAtMs
          ? new Date(job.state.nextRunAtMs).toLocaleString()
          : "\u2014";
        tr.appendChild(tdNext);

        var tdStatus = document.createElement("td");
        if (job.state && job.state.lastStatus) {
          var badge = document.createElement("span");
          badge.className = "cron-badge " + job.state.lastStatus;
          badge.textContent = job.state.lastStatus;
          tdStatus.appendChild(badge);
        } else {
          tdStatus.textContent = "\u2014";
        }
        tr.appendChild(tdStatus);

        var tdEnabled = document.createElement("td");
        var toggle = document.createElement("label");
        toggle.className = "cron-toggle";
        var checkbox = document.createElement("input");
        checkbox.type = "checkbox";
        checkbox.checked = job.enabled;
        checkbox.addEventListener("change", function () {
          sendRpc("cron.update", { id: job.id, patch: { enabled: checkbox.checked } }).then(function () {
            loadStatus();
          });
        });
        toggle.appendChild(checkbox);
        var slider = document.createElement("span");
        slider.className = "cron-slider";
        toggle.appendChild(slider);
        tdEnabled.appendChild(toggle);
        tr.appendChild(tdEnabled);

        var tdActions = document.createElement("td");
        tdActions.className = "cron-actions";

        var editBtn = document.createElement("button");
        editBtn.className = "cron-action-btn";
        editBtn.textContent = "Edit";
        editBtn.addEventListener("click", function () { openCronModal(job); });
        tdActions.appendChild(editBtn);

        var runBtn = document.createElement("button");
        runBtn.className = "cron-action-btn";
        runBtn.textContent = "Run";
        runBtn.addEventListener("click", function () {
          sendRpc("cron.run", { id: job.id, force: true }).then(function () {
            loadJobs();
            loadStatus();
          });
        });
        tdActions.appendChild(runBtn);

        var histBtn = document.createElement("button");
        histBtn.className = "cron-action-btn";
        histBtn.textContent = "History";
        histBtn.addEventListener("click", function () { showRunHistory(job.id, job.name); });
        tdActions.appendChild(histBtn);

        var delBtn = document.createElement("button");
        delBtn.className = "cron-action-btn cron-action-danger";
        delBtn.textContent = "Delete";
        delBtn.addEventListener("click", function () {
          if (confirm("Delete job '" + job.name + "'?")) {
            sendRpc("cron.remove", { id: job.id }).then(function () {
              loadJobs();
              loadStatus();
            });
          }
        });
        tdActions.appendChild(delBtn);

        tr.appendChild(tdActions);
        tbody.appendChild(tr);
      });
      table.appendChild(tbody);
      cronJobList.appendChild(table);
    }

    function formatSchedule(sched) {
      if (sched.kind === "at") return "At " + new Date(sched.atMs).toLocaleString();
      if (sched.kind === "every") {
        var ms = sched.everyMs;
        if (ms >= 3600000) return "Every " + (ms / 3600000) + "h";
        if (ms >= 60000) return "Every " + (ms / 60000) + "m";
        return "Every " + (ms / 1000) + "s";
      }
      if (sched.kind === "cron") return sched.expr + (sched.tz ? " (" + sched.tz + ")" : "");
      return JSON.stringify(sched);
    }

    function showRunHistory(jobId, jobName) {
      cronRunsPanel.classList.remove("hidden");
      cronRunsPanel.textContent = "";
      var loading = document.createElement("div");
      loading.className = "text-sm text-[var(--muted)]";
      loading.textContent = "Loading history for " + jobName + "...";
      cronRunsPanel.appendChild(loading);

      sendRpc("cron.runs", { id: jobId }).then(function (res) {
        cronRunsPanel.textContent = "";
        if (!res || !res.ok) {
          var errEl = document.createElement("div");
          errEl.className = "text-sm text-[var(--error)]";
          errEl.textContent = "Failed to load history";
          cronRunsPanel.appendChild(errEl);
          return;
        }
        var runs = res.payload || [];

        var header = document.createElement("div");
        header.className = "flex items-center justify-between";
        header.style.marginBottom = "8px";
        var titleEl = document.createElement("span");
        titleEl.className = "text-sm font-medium text-[var(--text-strong)]";
        titleEl.textContent = "Run History: " + jobName;
        header.appendChild(titleEl);
        var closeBtn = document.createElement("button");
        closeBtn.className = "text-xs text-[var(--muted)] cursor-pointer bg-transparent border-none hover:text-[var(--text)]";
        closeBtn.textContent = "\u2715 Close";
        closeBtn.addEventListener("click", function () { cronRunsPanel.classList.add("hidden"); });
        header.appendChild(closeBtn);
        cronRunsPanel.appendChild(header);

        if (runs.length === 0) {
          var emptyEl = document.createElement("div");
          emptyEl.className = "text-xs text-[var(--muted)]";
          emptyEl.textContent = "No runs yet.";
          cronRunsPanel.appendChild(emptyEl);
          return;
        }

        runs.forEach(function (run) {
          var item = document.createElement("div");
          item.className = "cron-run-item";

          var time = document.createElement("span");
          time.className = "text-xs text-[var(--muted)]";
          time.textContent = new Date(run.startedAtMs).toLocaleString();
          item.appendChild(time);

          var badge = document.createElement("span");
          badge.className = "cron-badge " + run.status;
          badge.textContent = run.status;
          item.appendChild(badge);

          var dur = document.createElement("span");
          dur.className = "text-xs text-[var(--muted)]";
          dur.textContent = run.durationMs + "ms";
          item.appendChild(dur);

          if (run.error) {
            var errSpan = document.createElement("span");
            errSpan.className = "text-xs text-[var(--error)]";
            errSpan.textContent = run.error;
            item.appendChild(errSpan);
          }

          cronRunsPanel.appendChild(item);
        });
      });
    }

    function openCronModal(existingJob) {
      var isEdit = !!existingJob;
      providerModal.classList.remove("hidden");
      providerModalTitle.textContent = isEdit ? "Edit Job" : "Add Job";
      providerModalBody.textContent = "";

      var form = document.createElement("div");
      form.className = "provider-key-form";

      function addField(labelText, el) {
        var lbl = document.createElement("label");
        lbl.className = "text-xs text-[var(--muted)]";
        lbl.textContent = labelText;
        form.appendChild(lbl);
        form.appendChild(el);
      }

      var nameInput = document.createElement("input");
      nameInput.className = "provider-key-input";
      nameInput.placeholder = "Job name";
      nameInput.value = isEdit ? existingJob.name : "";
      addField("Name", nameInput);

      var schedSelect = document.createElement("select");
      schedSelect.className = "provider-key-input";
      ["at", "every", "cron"].forEach(function (k) {
        var opt = document.createElement("option");
        opt.value = k;
        opt.textContent = k === "at" ? "At (one-shot)" : k === "every" ? "Every (interval)" : "Cron (expression)";
        schedSelect.appendChild(opt);
      });
      addField("Schedule Type", schedSelect);

      var schedParams = document.createElement("div");
      form.appendChild(schedParams);

      var schedAtInput = document.createElement("input");
      schedAtInput.className = "provider-key-input";
      schedAtInput.type = "datetime-local";

      var schedEveryInput = document.createElement("input");
      schedEveryInput.className = "provider-key-input";
      schedEveryInput.type = "number";
      schedEveryInput.placeholder = "Interval in seconds";
      schedEveryInput.min = "1";

      var schedCronInput = document.createElement("input");
      schedCronInput.className = "provider-key-input";
      schedCronInput.placeholder = "*/5 * * * *";

      var schedTzInput = document.createElement("input");
      schedTzInput.className = "provider-key-input";
      schedTzInput.placeholder = "Timezone (optional, e.g. Europe/Paris)";

      function updateSchedParams() {
        schedParams.textContent = "";
        var kind = schedSelect.value;
        if (kind === "at") {
          schedParams.appendChild(schedAtInput);
        } else if (kind === "every") {
          schedParams.appendChild(schedEveryInput);
        } else {
          schedParams.appendChild(schedCronInput);
          schedParams.appendChild(schedTzInput);
        }
      }
      schedSelect.addEventListener("change", updateSchedParams);

      var payloadSelect = document.createElement("select");
      payloadSelect.className = "provider-key-input";
      ["systemEvent", "agentTurn"].forEach(function (k) {
        var opt = document.createElement("option");
        opt.value = k;
        opt.textContent = k === "systemEvent" ? "System Event" : "Agent Turn";
        payloadSelect.appendChild(opt);
      });
      addField("Payload Type", payloadSelect);

      var payloadTextInput = document.createElement("textarea");
      payloadTextInput.className = "provider-key-input";
      payloadTextInput.placeholder = "Message text";
      payloadTextInput.style.minHeight = "60px";
      payloadTextInput.style.resize = "vertical";
      addField("Message", payloadTextInput);

      var targetSelect = document.createElement("select");
      targetSelect.className = "provider-key-input";
      ["isolated", "main"].forEach(function (k) {
        var opt = document.createElement("option");
        opt.value = k;
        opt.textContent = k.charAt(0).toUpperCase() + k.slice(1);
        targetSelect.appendChild(opt);
      });
      addField("Session Target", targetSelect);

      var deleteAfterLabel = document.createElement("label");
      deleteAfterLabel.className = "text-xs text-[var(--muted)] flex items-center gap-2";
      var deleteAfterCheck = document.createElement("input");
      deleteAfterCheck.type = "checkbox";
      deleteAfterLabel.appendChild(deleteAfterCheck);
      deleteAfterLabel.appendChild(document.createTextNode("Delete after run"));
      form.appendChild(deleteAfterLabel);

      var enabledLabel = document.createElement("label");
      enabledLabel.className = "text-xs text-[var(--muted)] flex items-center gap-2";
      var enabledCheck = document.createElement("input");
      enabledCheck.type = "checkbox";
      enabledCheck.checked = true;
      enabledLabel.appendChild(enabledCheck);
      enabledLabel.appendChild(document.createTextNode("Enabled"));
      form.appendChild(enabledLabel);

      if (isEdit) {
        var s = existingJob.schedule;
        schedSelect.value = s.kind;
        if (s.kind === "at" && s.atMs) {
          schedAtInput.value = new Date(s.atMs).toISOString().slice(0, 16);
        } else if (s.kind === "every" && s.everyMs) {
          schedEveryInput.value = Math.round(s.everyMs / 1000);
        } else if (s.kind === "cron") {
          schedCronInput.value = s.expr || "";
          schedTzInput.value = s.tz || "";
        }

        var p = existingJob.payload;
        payloadSelect.value = p.kind;
        payloadTextInput.value = p.text || p.message || "";
        targetSelect.value = existingJob.sessionTarget || "isolated";
        deleteAfterCheck.checked = existingJob.deleteAfterRun || false;
        enabledCheck.checked = existingJob.enabled;
      }

      updateSchedParams();

      var btns = document.createElement("div");
      btns.style.display = "flex";
      btns.style.gap = "8px";
      btns.style.marginTop = "8px";

      var cancelBtn = document.createElement("button");
      cancelBtn.className = "provider-btn provider-btn-secondary";
      cancelBtn.textContent = "Cancel";
      cancelBtn.addEventListener("click", closeProviderModal);
      btns.appendChild(cancelBtn);

      var saveBtn = document.createElement("button");
      saveBtn.className = "provider-btn";
      saveBtn.textContent = isEdit ? "Update" : "Create";
      saveBtn.addEventListener("click", function () {
        var name = nameInput.value.trim();
        if (!name) { nameInput.style.borderColor = "var(--error)"; return; }

        var schedule;
        var kind = schedSelect.value;
        if (kind === "at") {
          var ts = new Date(schedAtInput.value).getTime();
          if (isNaN(ts)) { schedAtInput.style.borderColor = "var(--error)"; return; }
          schedule = { kind: "at", atMs: ts };
        } else if (kind === "every") {
          var secs = parseInt(schedEveryInput.value, 10);
          if (isNaN(secs) || secs <= 0) { schedEveryInput.style.borderColor = "var(--error)"; return; }
          schedule = { kind: "every", everyMs: secs * 1000 };
        } else {
          var expr = schedCronInput.value.trim();
          if (!expr) { schedCronInput.style.borderColor = "var(--error)"; return; }
          schedule = { kind: "cron", expr: expr };
          var tz = schedTzInput.value.trim();
          if (tz) schedule.tz = tz;
        }

        var msgText = payloadTextInput.value.trim();
        if (!msgText) { payloadTextInput.style.borderColor = "var(--error)"; return; }
        var payload;
        if (payloadSelect.value === "systemEvent") {
          payload = { kind: "systemEvent", text: msgText };
        } else {
          payload = { kind: "agentTurn", message: msgText, deliver: false };
        }

        saveBtn.disabled = true;
        saveBtn.textContent = "Saving...";

        if (isEdit) {
          sendRpc("cron.update", { id: existingJob.id, patch: {
            name: name, schedule: schedule, payload: payload,
            sessionTarget: targetSelect.value,
            deleteAfterRun: deleteAfterCheck.checked,
            enabled: enabledCheck.checked
          }}).then(function (res) {
            if (res && res.ok) { closeProviderModal(); loadJobs(); loadStatus(); }
            else { saveBtn.disabled = false; saveBtn.textContent = "Update"; }
          });
        } else {
          sendRpc("cron.add", {
            name: name, schedule: schedule, payload: payload,
            sessionTarget: targetSelect.value,
            deleteAfterRun: deleteAfterCheck.checked,
            enabled: enabledCheck.checked
          }).then(function (res) {
            if (res && res.ok) { closeProviderModal(); loadJobs(); loadStatus(); }
            else { saveBtn.disabled = false; saveBtn.textContent = "Create"; }
          });
        }
      });
      btns.appendChild(saveBtn);
      form.appendChild(btns);

      providerModalBody.appendChild(form);
      nameInput.focus();
    }

    $("cronAddBtn").addEventListener("click", function () { openCronModal(null); });
    $("cronRefreshBtn").addEventListener("click", function () { loadJobs(); loadStatus(); });

    loadStatus();
    loadJobs();
  });

  // ════════════════════════════════════════════════════════════
  // Projects page
  // ════════════════════════════════════════════════════════════

  function createEl(tag, attrs, children) {
    var el = document.createElement(tag);
    if (attrs) {
      Object.keys(attrs).forEach(function (k) {
        if (k === "className") el.className = attrs[k];
        else if (k === "textContent") el.textContent = attrs[k];
        else if (k === "style") el.style.cssText = attrs[k];
        else el.setAttribute(k, attrs[k]);
      });
    }
    if (children) {
      children.forEach(function (c) { if (c) el.appendChild(c); });
    }
    return el;
  }

  registerPage("/projects", function initProjects(container) {
    var wrapper = createEl("div", { className: "flex-1 flex flex-col min-w-0 p-4 gap-4 overflow-y-auto" });

    var header = createEl("div", { className: "flex items-center gap-3" }, [
      createEl("h2", { className: "text-lg font-medium text-[var(--text-strong)]", textContent: "Projects" })
    ]);

    var detectBtn = createEl("button", {
      className: "text-xs text-[var(--muted)] border border-[var(--border)] px-2.5 py-1 rounded-md hover:text-[var(--text)] hover:border-[var(--border-strong)] transition-colors cursor-pointer bg-transparent",
      textContent: "Auto-detect"
    });
    header.appendChild(detectBtn);
    wrapper.appendChild(header);

    // Add project form
    var formRow = createEl("div", { className: "flex items-end gap-3", style: "max-width:600px;" });
    var dirGroup = createEl("div", { style: "flex:1;position:relative;" });
    var dirLabel = createEl("div", { className: "text-xs text-[var(--muted)]", textContent: "Directory", style: "margin-bottom:4px;" });
    dirGroup.appendChild(dirLabel);
    var dirInput = createEl("input", {
      type: "text",
      className: "provider-key-input",
      placeholder: "/path/to/project",
      style: "font-family:var(--font-mono);width:100%;"
    });
    dirGroup.appendChild(dirInput);

    var completionList = createEl("div", {
      style: "position:absolute;left:0;right:0;top:100%;background:var(--surface);border:1px solid var(--border);border-radius:4px;max-height:150px;overflow-y:auto;z-index:20;display:none;"
    });
    dirGroup.appendChild(completionList);
    formRow.appendChild(dirGroup);

    var addBtn = createEl("button", {
      className: "bg-[var(--accent-dim)] text-white border-none px-3 py-1.5 rounded text-xs cursor-pointer hover:bg-[var(--accent)] transition-colors",
      textContent: "Add",
      style: "height:34px;"
    });
    formRow.appendChild(addBtn);
    wrapper.appendChild(formRow);

    // Project list container
    var listEl = createEl("div", { style: "max-width:600px;margin-top:8px;" });
    wrapper.appendChild(listEl);
    container.appendChild(wrapper);

    // ── Directory autocomplete ──
    var completeTimer = null;
    dirInput.addEventListener("input", function () {
      clearTimeout(completeTimer);
      completeTimer = setTimeout(function () {
        var val = dirInput.value;
        if (val.length < 2) { completionList.style.display = "none"; return; }
        sendRpc("projects.complete_path", { partial: val }).then(function (res) {
          if (!res || !res.ok) { completionList.style.display = "none"; return; }
          var paths = res.payload || [];
          while (completionList.firstChild) completionList.removeChild(completionList.firstChild);
          if (paths.length === 0) { completionList.style.display = "none"; return; }
          paths.forEach(function (p) {
            var item = createEl("div", {
              textContent: p,
              style: "padding:6px 10px;cursor:pointer;font-size:.78rem;font-family:var(--font-mono);color:var(--text);transition:background .1s;"
            });
            item.addEventListener("mouseenter", function () { item.style.background = "var(--bg-hover)"; });
            item.addEventListener("mouseleave", function () { item.style.background = ""; });
            item.addEventListener("click", function () {
              dirInput.value = p + "/";
              completionList.style.display = "none";
              dirInput.focus();
              dirInput.dispatchEvent(new Event("input"));
            });
            completionList.appendChild(item);
          });
          completionList.style.display = "block";
        });
      }, 200);
    });

    // ── Render project list ──
    function renderList() {
      while (listEl.firstChild) listEl.removeChild(listEl.firstChild);
      if (projects.length === 0) {
        listEl.appendChild(createEl("div", {
          className: "text-xs text-[var(--muted)]",
          textContent: "No projects configured. Add a directory above or use auto-detect.",
          style: "padding:12px 0;"
        }));
        return;
      }
      projects.forEach(function (p) {
        var card = createEl("div", {
          className: "provider-item",
          style: "margin-bottom:6px;"
        });

        var info = createEl("div", { style: "flex:1;min-width:0;" });
        var nameRow = createEl("div", { className: "flex items-center gap-2" });
        nameRow.appendChild(createEl("div", { className: "provider-item-name", textContent: p.label || p.id }));
        if (p.detected) {
          nameRow.appendChild(createEl("span", { className: "provider-item-badge api-key", textContent: "auto" }));
        }
        if (p.auto_worktree) {
          nameRow.appendChild(createEl("span", { className: "provider-item-badge oauth", textContent: "worktree" }));
        }
        info.appendChild(nameRow);

        info.appendChild(createEl("div", {
          textContent: p.directory,
          style: "font-size:.72rem;color:var(--muted);font-family:var(--font-mono);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;margin-top:2px;"
        }));

        if (p.setup_command) {
          nameRow.appendChild(createEl("span", { className: "provider-item-badge api-key", textContent: "setup" }));
        }
        if (p.teardown_command) {
          nameRow.appendChild(createEl("span", { className: "provider-item-badge api-key", textContent: "teardown" }));
        }
        if (p.branch_prefix) {
          nameRow.appendChild(createEl("span", { className: "provider-item-badge oauth", textContent: p.branch_prefix + "/*" }));
        }

        if (p.system_prompt) {
          info.appendChild(createEl("div", {
            textContent: "System prompt: " + p.system_prompt.substring(0, 80) + (p.system_prompt.length > 80 ? "..." : ""),
            style: "font-size:.7rem;color:var(--muted);margin-top:2px;font-style:italic;"
          }));
        }

        card.appendChild(info);

        var actions = createEl("div", { style: "display:flex;gap:4px;flex-shrink:0;" });

        var editBtn = createEl("button", {
          className: "session-action-btn",
          textContent: "edit",
          title: "Edit project"
        });
        editBtn.addEventListener("click", function (e) {
          e.stopPropagation();
          showEditForm(p, card);
        });
        actions.appendChild(editBtn);

        var delBtn = createEl("button", {
          className: "session-action-btn session-delete",
          textContent: "x",
          title: "Remove project"
        });
        delBtn.addEventListener("click", function (e) {
          e.stopPropagation();
          sendRpc("projects.delete", { id: p.id }).then(function () {
            fetchProjects();
            setTimeout(renderList, 200);
          });
        });
        actions.appendChild(delBtn);

        card.appendChild(actions);
        listEl.appendChild(card);
      });
    }

    // ── Edit form (inline, replaces card) ──
    function showEditForm(p, cardEl) {
      var form = createEl("div", {
        style: "background:var(--surface2);border:1px solid var(--border);border-radius:6px;padding:12px;margin-bottom:6px;"
      });

      function labeledInput(labelText, value, placeholder, mono) {
        var group = createEl("div", { style: "margin-bottom:8px;" });
        group.appendChild(createEl("div", {
          className: "text-xs text-[var(--muted)]",
          textContent: labelText,
          style: "margin-bottom:3px;"
        }));
        var input = createEl("input", {
          type: "text",
          className: "provider-key-input",
          value: value || "",
          placeholder: placeholder || "",
          style: mono ? "font-family:var(--font-mono);width:100%;" : "width:100%;"
        });
        group.appendChild(input);
        return { group: group, input: input };
      }

      var labelField = labeledInput("Label", p.label, "Project name");
      form.appendChild(labelField.group);

      var dirField = labeledInput("Directory", p.directory, "/path/to/project", true);
      form.appendChild(dirField.group);

      var promptGroup = createEl("div", { style: "margin-bottom:8px;" });
      promptGroup.appendChild(createEl("div", {
        className: "text-xs text-[var(--muted)]",
        textContent: "System prompt (optional)",
        style: "margin-bottom:3px;"
      }));
      var promptInput = createEl("textarea", {
        className: "provider-key-input",
        placeholder: "Extra instructions for the LLM when working on this project...",
        style: "width:100%;min-height:60px;resize-y;font-size:.8rem;"
      });
      promptInput.value = p.system_prompt || "";
      promptGroup.appendChild(promptInput);
      form.appendChild(promptGroup);

      var setupField = labeledInput("Setup command", p.setup_command, "e.g. pnpm install", true);
      form.appendChild(setupField.group);

      var teardownField = labeledInput("Teardown command", p.teardown_command, "e.g. docker compose down", true);
      form.appendChild(teardownField.group);

      var prefixField = labeledInput("Branch prefix", p.branch_prefix, "default: moltis", true);
      form.appendChild(prefixField.group);

      // Worktree toggle
      var wtGroup = createEl("div", { style: "margin-bottom:10px;display:flex;align-items:center;gap:8px;" });
      var wtCheckbox = createEl("input", { type: "checkbox" });
      wtCheckbox.checked = p.auto_worktree;
      wtGroup.appendChild(wtCheckbox);
      wtGroup.appendChild(createEl("span", {
        className: "text-xs text-[var(--text)]",
        textContent: "Auto-create git worktree per session"
      }));
      form.appendChild(wtGroup);

      var btnRow = createEl("div", { style: "display:flex;gap:8px;" });
      var saveBtn = createEl("button", { className: "provider-btn", textContent: "Save" });
      var cancelBtn = createEl("button", { className: "provider-btn provider-btn-secondary", textContent: "Cancel" });

      saveBtn.addEventListener("click", function () {
        var updated = JSON.parse(JSON.stringify(p));
        updated.label = labelField.input.value.trim() || p.label;
        updated.directory = dirField.input.value.trim() || p.directory;
        updated.system_prompt = promptInput.value.trim() || null;
        updated.setup_command = setupField.input.value.trim() || null;
        updated.teardown_command = teardownField.input.value.trim() || null;
        updated.branch_prefix = prefixField.input.value.trim() || null;
        updated.auto_worktree = wtCheckbox.checked;
        updated.updated_at = Date.now();

        sendRpc("projects.upsert", updated).then(function () {
          fetchProjects();
          setTimeout(renderList, 200);
        });
      });

      cancelBtn.addEventListener("click", function () {
        listEl.replaceChild(cardEl, form);
      });

      btnRow.appendChild(saveBtn);
      btnRow.appendChild(cancelBtn);
      form.appendChild(btnRow);

      listEl.replaceChild(form, cardEl);
    }

    // ── Add project ──
    addBtn.addEventListener("click", function () {
      var dir = dirInput.value.trim();
      if (!dir) return;
      addBtn.disabled = true;
      sendRpc("projects.detect", { directories: [dir] }).then(function (res) {
        addBtn.disabled = false;
        if (res && res.ok) {
          var detected = res.payload || [];
          if (detected.length === 0) {
            var slug = dir.split("/").filter(Boolean).pop() || "project";
            var now = Date.now();
            sendRpc("projects.upsert", {
              id: slug.toLowerCase().replace(/[^a-z0-9-]/g, "-"),
              label: slug,
              directory: dir,
              auto_worktree: false,
              detected: false,
              created_at: now,
              updated_at: now
            }).then(function () {
              dirInput.value = "";
              fetchProjects();
              setTimeout(renderList, 200);
            });
          } else {
            dirInput.value = "";
            fetchProjects();
            setTimeout(renderList, 200);
          }
        }
      });
    });

    // ── Auto-detect ──
    detectBtn.addEventListener("click", function () {
      detectBtn.disabled = true;
      detectBtn.textContent = "Detecting...";
      sendRpc("projects.detect", { directories: [] }).then(function () {
        detectBtn.disabled = false;
        detectBtn.textContent = "Auto-detect";
        fetchProjects();
        setTimeout(renderList, 200);
      });
    });

    renderList();
  });

  // ════════════════════════════════════════════════════════════
  // Providers page
  // ════════════════════════════════════════════════════════════
  // Safe: static hardcoded HTML template, no user input.
  var providersPageHTML =
    '<div class="flex-1 flex flex-col min-w-0 p-4 gap-4 overflow-y-auto">' +
      '<div class="flex items-center gap-3">' +
        '<h2 class="text-lg font-medium text-[var(--text-strong)]">Providers</h2>' +
        '<button id="provAddBtn" class="bg-[var(--accent-dim)] text-white border-none px-3 py-1.5 rounded text-xs cursor-pointer hover:bg-[var(--accent)] transition-colors">+ Add Provider</button>' +
      '</div>' +
      '<div id="providerPageList"></div>' +
    '</div>';

  registerPage("/providers", function initProviders(container) {
    container.innerHTML = providersPageHTML;

    var addBtn = $("provAddBtn");
    var listEl = $("providerPageList");

    addBtn.addEventListener("click", function () {
      if (connected) openProviderModal();
    });

    function renderProviderList() {
      sendRpc("providers.available", {}).then(function (res) {
        if (!res || !res.ok) return;
        var providers = res.payload || [];
        while (listEl.firstChild) listEl.removeChild(listEl.firstChild);

        if (providers.length === 0) {
          listEl.appendChild(createEl("div", {
            className: "text-sm text-[var(--muted)]",
            textContent: "No providers available."
          }));
          return;
        }

        providers.forEach(function (p) {
          var card = createEl("div", {
            style: "display:flex;align-items:center;justify-content:space-between;padding:10px 12px;border:1px solid var(--border);border-radius:6px;margin-bottom:6px;" +
              (p.configured ? "" : "opacity:0.5;")
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

          if (p.configured) {
            left.appendChild(createEl("span", {
              className: "provider-item-badge configured",
              textContent: "configured"
            }));
          }

          card.appendChild(left);

          if (p.configured) {
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
          } else {
            var connectBtn = createEl("button", {
              className: "bg-[var(--accent-dim)] text-white border-none px-2.5 py-1 rounded text-xs cursor-pointer hover:bg-[var(--accent)] transition-colors",
              textContent: "Connect"
            });
            connectBtn.addEventListener("click", function () {
              if (p.authType === "api-key") showApiKeyForm(p);
              else if (p.authType === "oauth") showOAuthFlow(p);
            });
            card.appendChild(connectBtn);
          }

          listEl.appendChild(card);
        });
      });
    }

    refreshProvidersPage = renderProviderList;
    renderProviderList();
  }, function teardownProviders() {
    refreshProvidersPage = null;
  });

  // ════════════════════════════════════════════════════════════
  // Channels page
  // ════════════════════════════════════════════════════════════
  var channelModal = $("channelModal");
  var channelModalTitle = $("channelModalTitle");
  var channelModalBody = $("channelModalBody");
  var channelModalClose = $("channelModalClose");

  function makeTelegramIcon() {
    var ns = "http://www.w3.org/2000/svg";
    var svg = document.createElementNS(ns, "svg");
    svg.setAttribute("width", "16");
    svg.setAttribute("height", "16");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "1.5");
    var path = document.createElementNS(ns, "path");
    path.setAttribute("d", "M22 2L11 13M22 2l-7 20-4-9-9-4 20-7z");
    svg.appendChild(path);
    return svg;
  }

  function openChannelModal(onAdded) {
    channelModal.classList.remove("hidden");
    channelModalTitle.textContent = "Add Telegram Bot";
    channelModalBody.textContent = "";

    var form = createEl("div", { style: "display:flex;flex-direction:column;gap:12px;padding:8px 0;" });

    // Instructions
    var helpBox = createEl("div", {
      style: "background:var(--surface2);border:1px solid var(--border);border-radius:6px;padding:10px 12px;display:flex;flex-direction:column;gap:6px;"
    });
    var helpTitle = createEl("span", {
      className: "text-xs font-medium text-[var(--text-strong)]",
      textContent: "How to create a Telegram bot"
    });
    helpBox.appendChild(helpTitle);

    var step1 = createEl("div", { className: "text-xs text-[var(--muted)]", style: "display:flex;gap:4px;" });
    step1.appendChild(document.createTextNode("1. Open "));
    var bfLink = createEl("a", {
      href: "https://t.me/BotFather",
      target: "_blank",
      className: "text-[var(--accent)]",
      style: "text-decoration:underline;",
      textContent: "@BotFather"
    });
    step1.appendChild(bfLink);
    step1.appendChild(document.createTextNode(" in Telegram"));
    helpBox.appendChild(step1);

    helpBox.appendChild(createEl("div", {
      className: "text-xs text-[var(--muted)]",
      textContent: "2. Send /newbot and follow the prompts to choose a name and username"
    }));
    helpBox.appendChild(createEl("div", {
      className: "text-xs text-[var(--muted)]",
      textContent: "3. Copy the bot token (looks like 123456:ABC-DEF...) and paste it below"
    }));

    var helpTip = createEl("div", { className: "text-xs text-[var(--muted)]", style: "display:flex;gap:4px;margin-top:2px;" });
    helpTip.appendChild(document.createTextNode("See the "));
    var docsLink = createEl("a", {
      href: "https://core.telegram.org/bots/tutorial",
      target: "_blank",
      className: "text-[var(--accent)]",
      style: "text-decoration:underline;",
      textContent: "Telegram Bot Tutorial"
    });
    helpTip.appendChild(docsLink);
    helpTip.appendChild(document.createTextNode(" for more details."));
    helpBox.appendChild(helpTip);

    form.appendChild(helpBox);

    // Bot username
    var idLabel = createEl("label", { className: "text-xs text-[var(--muted)]", textContent: "Bot username" });
    var idInput = createEl("input", {
      type: "text",
      placeholder: "e.g. my_assistant_bot",
      className: "text-sm bg-[var(--surface2)] border border-[var(--border)] rounded px-3 py-2 text-[var(--text)] focus:outline-none focus:border-[var(--border-strong)]",
      style: "font-family:var(--font-body);"
    });

    // Bot Token
    var tokenLabel = createEl("label", { className: "text-xs text-[var(--muted)]", textContent: "Bot Token (from @BotFather)" });
    var tokenInput = createEl("input", {
      type: "password",
      placeholder: "123456:ABC-DEF...",
      className: "text-sm bg-[var(--surface2)] border border-[var(--border)] rounded px-3 py-2 text-[var(--text)] focus:outline-none focus:border-[var(--border-strong)]",
      style: "font-family:var(--font-body);"
    });

    // DM Policy
    var dmLabel = createEl("label", { className: "text-xs text-[var(--muted)]", textContent: "DM Policy" });
    var dmSelect = createEl("select", {
      className: "text-sm bg-[var(--surface2)] border border-[var(--border)] rounded px-3 py-2 text-[var(--text)] cursor-pointer",
      style: "font-family:var(--font-body);"
    });
    [["open", "Open (anyone)"], ["allowlist", "Allowlist only"], ["disabled", "Disabled"]].forEach(function (opt) {
      var o = createEl("option", { value: opt[0], textContent: opt[1] });
      dmSelect.appendChild(o);
    });

    // Mention Mode
    var mentionLabel = createEl("label", { className: "text-xs text-[var(--muted)]", textContent: "Group Mention Mode" });
    var mentionSelect = createEl("select", {
      className: "text-sm bg-[var(--surface2)] border border-[var(--border)] rounded px-3 py-2 text-[var(--text)] cursor-pointer",
      style: "font-family:var(--font-body);"
    });
    [["mention", "Must @mention bot"], ["always", "Always respond"], ["none", "Don't respond in groups"]].forEach(function (opt) {
      var o = createEl("option", { value: opt[0], textContent: opt[1] });
      mentionSelect.appendChild(o);
    });

    // Default Model
    var modelLabel = createEl("label", { className: "text-xs text-[var(--muted)]", textContent: "Default Model" });
    var modelSelect = createEl("select", {
      className: "text-sm bg-[var(--surface2)] border border-[var(--border)] rounded px-3 py-2 text-[var(--text)] cursor-pointer",
      style: "font-family:var(--font-body);"
    });
    var defaultOpt = createEl("option", { value: "", textContent: "(server default)" });
    modelSelect.appendChild(defaultOpt);
    models.forEach(function (m) {
      var o = createEl("option", { value: m.id, textContent: m.displayName || m.id });
      modelSelect.appendChild(o);
    });

    // Allowlist
    var allowLabel = createEl("label", { className: "text-xs text-[var(--muted)]", textContent: "DM Allowlist (one username per line)" });
    var allowInput = createEl("textarea", {
      placeholder: "user1\nuser2",
      rows: 3,
      className: "text-sm bg-[var(--surface2)] border border-[var(--border)] rounded px-3 py-2 text-[var(--text)] focus:outline-none focus:border-[var(--border-strong)]",
      style: "font-family:var(--font-body);resize:vertical;"
    });

    // Error display
    var errorEl = createEl("div", {
      className: "text-xs text-[var(--error)]",
      style: "display:none;padding:4px 0;"
    });

    // Submit button
    var submitBtn = createEl("button", {
      className: "bg-[var(--accent-dim)] text-white border-none px-4 py-2 rounded text-sm cursor-pointer hover:bg-[var(--accent)] transition-colors",
      textContent: "Connect Bot"
    });

    submitBtn.addEventListener("click", function () {
      var accountId = idInput.value.trim();
      var token = tokenInput.value.trim();

      if (!accountId) {
        errorEl.textContent = "Bot username is required.";
        errorEl.style.display = "block";
        return;
      }
      if (!token) {
        errorEl.textContent = "Bot token is required.";
        errorEl.style.display = "block";
        return;
      }

      var allowlist = allowInput.value.trim().split(/\n/).map(function(s){ return s.trim(); }).filter(Boolean);

      errorEl.style.display = "none";
      submitBtn.disabled = true;
      submitBtn.textContent = "Connecting...";

      var addConfig = {
          token: token,
          dm_policy: dmSelect.value,
          mention_mode: mentionSelect.value,
          allowlist: allowlist
      };
      if (modelSelect.value) addConfig.model = modelSelect.value;

      sendRpc("channels.add", {
        type: "telegram",
        account_id: accountId,
        config: addConfig
      }).then(function (res) {
        submitBtn.disabled = false;
        submitBtn.textContent = "Connect Bot";
        if (res && res.ok) {
          closeChannelModal();
          if (onAdded) onAdded();
        } else {
          var msg = (res && res.error && (res.error.message || res.error.detail)) || "Failed to connect bot.";
          errorEl.textContent = msg;
          errorEl.style.display = "block";
        }
      });
    });

    form.appendChild(idLabel);
    form.appendChild(idInput);
    form.appendChild(tokenLabel);
    form.appendChild(tokenInput);
    form.appendChild(dmLabel);
    form.appendChild(dmSelect);
    form.appendChild(mentionLabel);
    form.appendChild(mentionSelect);
    form.appendChild(modelLabel);
    form.appendChild(modelSelect);
    form.appendChild(allowLabel);
    form.appendChild(allowInput);
    form.appendChild(errorEl);
    form.appendChild(submitBtn);

    channelModalBody.appendChild(form);
    idInput.focus();
  }

  function closeChannelModal() {
    channelModal.classList.add("hidden");
  }

  function openEditChannelModal(ch, onUpdated) {
    channelModal.classList.remove("hidden");
    channelModalTitle.textContent = "Edit Telegram Bot";
    channelModalBody.textContent = "";

    var cfg = ch.config || {};
    var form = createEl("div", { style: "display:flex;flex-direction:column;gap:12px;padding:8px 0;" });

    var nameEl = createEl("div", {
      className: "text-sm text-[var(--text-strong)]",
      textContent: ch.name || ch.account_id
    });
    form.appendChild(nameEl);

    // DM Policy
    var dmLabel = createEl("label", { className: "text-xs text-[var(--muted)]", textContent: "DM Policy" });
    var dmSelect = createEl("select", {
      className: "text-sm bg-[var(--surface2)] border border-[var(--border)] rounded px-3 py-2 text-[var(--text)] cursor-pointer",
      style: "font-family:var(--font-body);"
    });
    [["open", "Open (anyone)"], ["allowlist", "Allowlist only"], ["disabled", "Disabled"]].forEach(function (opt) {
      var o = createEl("option", { value: opt[0], textContent: opt[1] });
      if (opt[0] === cfg.dm_policy) o.selected = true;
      dmSelect.appendChild(o);
    });

    // Mention Mode
    var mentionLabel = createEl("label", { className: "text-xs text-[var(--muted)]", textContent: "Group Mention Mode" });
    var mentionSelect = createEl("select", {
      className: "text-sm bg-[var(--surface2)] border border-[var(--border)] rounded px-3 py-2 text-[var(--text)] cursor-pointer",
      style: "font-family:var(--font-body);"
    });
    [["mention", "Must @mention bot"], ["always", "Always respond"], ["none", "Don't respond in groups"]].forEach(function (opt) {
      var o = createEl("option", { value: opt[0], textContent: opt[1] });
      if (opt[0] === cfg.mention_mode) o.selected = true;
      mentionSelect.appendChild(o);
    });

    // Default Model
    var editModelLabel = createEl("label", { className: "text-xs text-[var(--muted)]", textContent: "Default Model" });
    var editModelSelect = createEl("select", {
      className: "text-sm bg-[var(--surface2)] border border-[var(--border)] rounded px-3 py-2 text-[var(--text)] cursor-pointer",
      style: "font-family:var(--font-body);"
    });
    var editDefaultOpt = createEl("option", { value: "", textContent: "(server default)" });
    editModelSelect.appendChild(editDefaultOpt);
    models.forEach(function (m) {
      var o = createEl("option", { value: m.id, textContent: m.displayName || m.id });
      if (m.id === cfg.model) o.selected = true;
      editModelSelect.appendChild(o);
    });

    // Allowlist
    var allowLabel = createEl("label", { className: "text-xs text-[var(--muted)]", textContent: "DM Allowlist (one username per line)" });
    var allowInput = createEl("textarea", {
      rows: 3,
      className: "text-sm bg-[var(--surface2)] border border-[var(--border)] rounded px-3 py-2 text-[var(--text)] focus:outline-none focus:border-[var(--border-strong)]",
      style: "font-family:var(--font-body);resize:vertical;"
    });
    allowInput.value = (cfg.allowlist || []).join("\n");

    // Error display
    var errorEl = createEl("div", {
      className: "text-xs text-[var(--error)]",
      style: "display:none;padding:4px 0;"
    });

    // Save button
    var saveBtn = createEl("button", {
      className: "bg-[var(--accent-dim)] text-white border-none px-4 py-2 rounded text-sm cursor-pointer hover:bg-[var(--accent)] transition-colors",
      textContent: "Save Changes"
    });

    saveBtn.addEventListener("click", function () {
      var allowlist = allowInput.value.trim().split(/\n/).map(function(s){ return s.trim(); }).filter(Boolean);

      errorEl.style.display = "none";
      saveBtn.disabled = true;
      saveBtn.textContent = "Saving...";

      var updateConfig = {
          token: cfg.token || "",
          dm_policy: dmSelect.value,
          mention_mode: mentionSelect.value,
          allowlist: allowlist
      };
      if (editModelSelect.value) updateConfig.model = editModelSelect.value;

      sendRpc("channels.update", {
        account_id: ch.account_id,
        config: updateConfig
      }).then(function (res) {
        saveBtn.disabled = false;
        saveBtn.textContent = "Save Changes";
        if (res && res.ok) {
          closeChannelModal();
          if (onUpdated) onUpdated();
        } else {
          var msg = (res && res.error && (res.error.message || res.error.detail)) || "Failed to update bot.";
          errorEl.textContent = msg;
          errorEl.style.display = "block";
        }
      });
    });

    form.appendChild(dmLabel);
    form.appendChild(dmSelect);
    form.appendChild(mentionLabel);
    form.appendChild(mentionSelect);
    form.appendChild(editModelLabel);
    form.appendChild(editModelSelect);
    form.appendChild(allowLabel);
    form.appendChild(allowInput);
    form.appendChild(errorEl);
    form.appendChild(saveBtn);

    channelModalBody.appendChild(form);
  }

  channelModalClose.addEventListener("click", closeChannelModal);
  channelModal.addEventListener("click", function (e) {
    if (e.target === channelModal) closeChannelModal();
  });

  // Safe: static hardcoded HTML template, no user input.
  var channelsPageHTML =
    '<div class="flex-1 flex flex-col min-w-0 p-4 gap-4 overflow-y-auto">' +
      '<div class="flex items-center gap-3">' +
        '<h2 class="text-lg font-medium text-[var(--text-strong)]">Channels</h2>' +
        '<div style="display:flex;gap:4px;margin-left:12px;">' +
          '<button id="chanTabChannels" class="session-action-btn" style="font-weight:600;">Channels</button>' +
          '<button id="chanTabSenders" class="session-action-btn">Senders</button>' +
        '</div>' +
        '<button id="chanAddBtn" class="bg-[var(--accent-dim)] text-white border-none px-3 py-1.5 rounded text-xs cursor-pointer hover:bg-[var(--accent)] transition-colors">+ Add Telegram Bot</button>' +
      '</div>' +
      '<div id="channelPageList"></div>' +
      '<div id="sendersPageContent" style="display:none;">' +
        '<div style="margin-bottom:12px;">' +
          '<label class="text-xs text-[var(--muted)]" style="margin-right:6px;">Account:</label>' +
          '<select id="sendersAccountSelect" style="background:var(--surface2);color:var(--text);border:1px solid var(--border);border-radius:4px;padding:4px 8px;font-size:12px;"></select>' +
        '</div>' +
        '<div id="sendersTableWrap"></div>' +
      '</div>' +
    '</div>';

  var refreshChannelsPage = null;
  var channelEventUnsub = null;

  registerPage("/channels", function initChannels(container) {
    // Safe: static hardcoded HTML template, no user input.
    container.innerHTML = channelsPageHTML; // eslint-disable-line -- safe: hardcoded template

    var addBtn = $("chanAddBtn");
    var listEl = $("channelPageList");
    var sendersContent = $("sendersPageContent");
    var tabChannels = $("chanTabChannels");
    var tabSenders = $("chanTabSenders");
    var sendersSelect = $("sendersAccountSelect");
    var sendersTableWrap = $("sendersTableWrap");
    var activeTab = "channels";

    // Real-time channel event listener (replaces polling).
    channelEventUnsub = onEvent("channel", function (p) {
      if (p.kind === "inbound_message" && activeTab === "senders" && sendersSelect.value === p.account_id) {
        loadSenders();
      }
    });

    function switchTab(tab) {
      activeTab = tab;
      if (tab === "channels") {
        listEl.style.display = "";
        sendersContent.style.display = "none";
        addBtn.style.display = "";
        tabChannels.style.fontWeight = "600";
        tabSenders.style.fontWeight = "";
        renderChannelList();
      } else {
        listEl.style.display = "none";
        sendersContent.style.display = "";
        addBtn.style.display = "none";
        tabChannels.style.fontWeight = "";
        tabSenders.style.fontWeight = "600";
        loadSendersAccounts();
      }
    }

    tabChannels.addEventListener("click", function () { switchTab("channels"); });
    tabSenders.addEventListener("click", function () { switchTab("senders"); });

    addBtn.addEventListener("click", function () {
      if (connected) openChannelModal(renderChannelList);
    });

    // ── Senders tab ──────────────────────────────────────────
    function loadSendersAccounts() {
      sendRpc("channels.status", {}).then(function (res) {
        if (!res || !res.ok) return;
        var channels = (res.payload && res.payload.channels) || [];
        while (sendersSelect.firstChild) sendersSelect.removeChild(sendersSelect.firstChild);
        if (channels.length === 0) {
          sendersTableWrap.textContent = "No channels configured.";
          return;
        }
        channels.forEach(function (ch) {
          var opt = document.createElement("option");
          opt.value = ch.account_id;
          opt.textContent = ch.name || ch.account_id;
          sendersSelect.appendChild(opt);
        });
        loadSenders();
      });
    }

    sendersSelect.addEventListener("change", loadSenders);

    function loadSenders() {
      var accountId = sendersSelect.value;
      if (!accountId) return;
      sendRpc("channels.senders.list", { account_id: accountId }).then(function (res) {
        if (!res || !res.ok) { sendersTableWrap.textContent = "Failed to load senders."; return; }
        var senders = (res.payload && res.payload.senders) || [];
        while (sendersTableWrap.firstChild) sendersTableWrap.removeChild(sendersTableWrap.firstChild);

        if (senders.length === 0) {
          sendersTableWrap.appendChild(createEl("div", {
            className: "text-sm text-[var(--muted)]",
            style: "text-align:center;padding:30px 0;",
            textContent: "No messages received yet for this account."
          }));
          return;
        }

        var table = createEl("table", {
          style: "width:100%;border-collapse:collapse;font-size:13px;"
        });
        var thead = document.createElement("thead");
        var headerRow = document.createElement("tr");
        ["Sender", "Username", "Messages", "Last Seen", "Status", "Action"].forEach(function (h) {
          var th = createEl("th", {
            textContent: h,
            style: "text-align:left;padding:6px 10px;border-bottom:1px solid var(--border);font-weight:500;color:var(--muted);font-size:11px;text-transform:uppercase;"
          });
          headerRow.appendChild(th);
        });
        thead.appendChild(headerRow);
        table.appendChild(thead);

        var tbody = document.createElement("tbody");
        senders.forEach(function (s) {
          var tr = document.createElement("tr");
          tr.style.cssText = "border-bottom:1px solid var(--border);";

          tr.appendChild(createEl("td", {
            style: "padding:8px 10px;",
            textContent: s.sender_name || s.peer_id
          }));

          tr.appendChild(createEl("td", {
            style: "padding:8px 10px;color:var(--muted);",
            textContent: s.username ? "@" + s.username : "\u2014"
          }));

          tr.appendChild(createEl("td", {
            style: "padding:8px 10px;",
            textContent: String(s.message_count)
          }));

          var lastSeen = s.last_seen ? new Date(s.last_seen * 1000).toLocaleString() : "\u2014";
          tr.appendChild(createEl("td", {
            style: "padding:8px 10px;color:var(--muted);font-size:12px;",
            textContent: lastSeen
          }));

          var statusTd = document.createElement("td");
          statusTd.style.cssText = "padding:8px 10px;";
          statusTd.appendChild(createEl("span", {
            className: "provider-item-badge " + (s.allowed ? "configured" : "oauth"),
            textContent: s.allowed ? "Allowed" : "Denied"
          }));
          tr.appendChild(statusTd);

          var actionTd = document.createElement("td");
          actionTd.style.cssText = "padding:8px 10px;";
          var identifier = s.username || s.peer_id;
          if (s.allowed) {
            var denyBtn = createEl("button", {
              className: "session-action-btn session-delete",
              textContent: "Deny",
              title: "Remove from allowlist"
            });
            denyBtn.addEventListener("click", function () {
              sendRpc("channels.senders.deny", {
                account_id: accountId,
                identifier: identifier
              }).then(function () { loadSenders(); });
            });
            actionTd.appendChild(denyBtn);
          } else {
            var approveBtn = createEl("button", {
              className: "session-action-btn",
              textContent: "Approve",
              title: "Add to allowlist"
            });
            approveBtn.style.cssText = "background:var(--accent-dim);color:white;";
            approveBtn.addEventListener("click", function () {
              sendRpc("channels.senders.approve", {
                account_id: accountId,
                identifier: identifier
              }).then(function () { loadSenders(); });
            });
            actionTd.appendChild(approveBtn);
          }
          tr.appendChild(actionTd);
          tbody.appendChild(tr);
        });
        table.appendChild(tbody);
        sendersTableWrap.appendChild(table);
      });
    }

    // ── Channels tab ─────────────────────────────────────────
    function renderChannelList() {
      sendRpc("channels.status", {}).then(function (res) {
        if (!res || !res.ok) return;
        var channels = (res.payload && res.payload.channels) || [];
        while (listEl.firstChild) listEl.removeChild(listEl.firstChild);

        if (channels.length === 0) {
          var empty = createEl("div", {
            style: "text-align:center;padding:40px 0;"
          });
          empty.appendChild(createEl("div", {
            className: "text-sm text-[var(--muted)]",
            style: "margin-bottom:12px;",
            textContent: "No Telegram bots connected."
          }));
          empty.appendChild(createEl("div", {
            className: "text-xs text-[var(--muted)]",
            textContent: "Click \"+ Add Telegram Bot\" to connect one using a token from @BotFather."
          }));
          listEl.appendChild(empty);
          return;
        }

        channels.forEach(function (ch) {
          var card = createEl("div", {
            style: "display:flex;align-items:center;justify-content:space-between;padding:12px 14px;border:1px solid var(--border);border-radius:8px;margin-bottom:8px;"
          });

          var left = createEl("div", { style: "display:flex;align-items:center;gap:10px;" });

          // Telegram icon
          var icon = createEl("span", {
            style: "display:inline-flex;align-items:center;justify-content:center;width:28px;height:28px;border-radius:6px;background:var(--surface2);"
          });
          icon.appendChild(makeTelegramIcon());
          left.appendChild(icon);

          var info = createEl("div", { style: "display:flex;flex-direction:column;gap:2px;" });
          info.appendChild(createEl("span", {
            className: "text-sm text-[var(--text-strong)]",
            textContent: ch.name || ch.account_id || "Telegram"
          }));

          if (ch.details) {
            info.appendChild(createEl("span", {
              className: "text-xs text-[var(--muted)]",
              textContent: ch.details
            }));
          }

          left.appendChild(info);

          // Status badge
          var statusClass = ch.status === "connected" ? "configured" : "oauth";
          var statusBadge = createEl("span", {
            className: "provider-item-badge " + statusClass,
            textContent: ch.status || "unknown"
          });
          left.appendChild(statusBadge);
          card.appendChild(left);

          var actions = createEl("div", { style: "display:flex;gap:6px;" });

          // Edit button
          var editBtn = createEl("button", {
            className: "session-action-btn",
            textContent: "Edit",
            title: "Edit " + (ch.account_id || "channel")
          });
          editBtn.addEventListener("click", function () {
            openEditChannelModal(ch, renderChannelList);
          });
          actions.appendChild(editBtn);

          // Remove button
          var removeBtn = createEl("button", {
            className: "session-action-btn session-delete",
            textContent: "Remove",
            title: "Remove " + (ch.account_id || "channel")
          });
          removeBtn.addEventListener("click", function () {
            if (!confirm("Remove " + (ch.name || ch.account_id) + "?")) return;
            sendRpc("channels.remove", { account_id: ch.account_id }).then(function (r) {
              if (r && r.ok) renderChannelList();
            });
          });
          actions.appendChild(removeBtn);
          card.appendChild(actions);

          listEl.appendChild(card);
        });
      });
    }

    refreshChannelsPage = renderChannelList;
    renderChannelList();
  }, function teardownChannels() {
    refreshChannelsPage = null;
    if (channelEventUnsub) { channelEventUnsub(); channelEventUnsub = null; }
  });

  // ── Event bus (pub/sub for WebSocket events) ─────────────
  var eventListeners = {};

  function onEvent(eventName, handler) {
    (eventListeners[eventName] = eventListeners[eventName] || []).push(handler);
    return function off() {
      var arr = eventListeners[eventName];
      if (arr) {
        var idx = arr.indexOf(handler);
        if (idx !== -1) arr.splice(idx, 1);
      }
    };
  }

  // ── Logs page ──────────────────────────────────────────────
  var logsEventHandler = null;
  var logsAlertDot = $("logsAlertDot");
  var unseenErrors = 0;
  var unseenWarns = 0;

  function updateLogsAlert() {
    if (unseenErrors > 0) {
      logsAlertDot.style.display = "";
      logsAlertDot.style.background = "var(--error)";
    } else if (unseenWarns > 0) {
      logsAlertDot.style.display = "";
      logsAlertDot.style.background = "var(--warn)";
    } else {
      logsAlertDot.style.display = "none";
    }
  }

  function clearLogsAlert() {
    unseenErrors = 0;
    unseenWarns = 0;
    updateLogsAlert();
    // Tell the server we've seen the logs
    if (connected) sendRpc("logs.ack", {});
  }

  registerPage("/logs", function initLogs(container) {
    var paused = false;
    var maxEntries = 2000;

    container.style.cssText = "flex-direction:column;padding:0;overflow:hidden;";

    // Toolbar
    var toolbar = document.createElement("div");
    toolbar.style.cssText = "display:flex;align-items:center;gap:8px;padding:8px 16px;border-bottom:1px solid var(--border);flex-shrink:0;flex-wrap:wrap;";

    // Level filter
    var levelSelect = document.createElement("select");
    levelSelect.style.cssText = "background:var(--surface2);border:1px solid var(--border);color:var(--text);border-radius:var(--radius-sm);font-size:.78rem;padding:4px 8px;font-family:var(--font-body);";
    var allOpt = document.createElement("option");
    allOpt.value = "";
    allOpt.textContent = "All levels";
    allOpt.selected = true;
    levelSelect.appendChild(allOpt);
    ["trace","debug","info","warn","error"].forEach(function (lvl) {
      var opt = document.createElement("option");
      opt.value = lvl;
      opt.textContent = lvl.toUpperCase();
      levelSelect.appendChild(opt);
    });

    // Target filter
    var targetInput = document.createElement("input");
    targetInput.type = "text";
    targetInput.placeholder = "Filter target…";
    targetInput.style.cssText = "background:var(--surface2);border:1px solid var(--border);color:var(--text);border-radius:var(--radius-sm);font-size:.78rem;padding:4px 8px;width:140px;font-family:var(--font-body);";

    // Search
    var searchInput = document.createElement("input");
    searchInput.type = "text";
    searchInput.placeholder = "Search…";
    searchInput.style.cssText = "background:var(--surface2);border:1px solid var(--border);color:var(--text);border-radius:var(--radius-sm);font-size:.78rem;padding:4px 8px;width:160px;font-family:var(--font-body);";

    // Buttons
    var pauseBtn = document.createElement("button");
    pauseBtn.textContent = "Pause";
    pauseBtn.style.cssText = "background:var(--surface2);border:1px solid var(--border);color:var(--text);border-radius:var(--radius-sm);font-size:.78rem;padding:4px 10px;cursor:pointer;";

    var clearBtn = document.createElement("button");
    clearBtn.textContent = "Clear";
    clearBtn.style.cssText = "background:var(--surface2);border:1px solid var(--border);color:var(--text);border-radius:var(--radius-sm);font-size:.78rem;padding:4px 10px;cursor:pointer;";

    var countLabel = document.createElement("span");
    countLabel.style.cssText = "color:var(--muted);font-size:.72rem;margin-left:auto;";
    countLabel.textContent = "0 entries";

    toolbar.appendChild(levelSelect);
    toolbar.appendChild(targetInput);
    toolbar.appendChild(searchInput);
    toolbar.appendChild(pauseBtn);
    toolbar.appendChild(clearBtn);
    toolbar.appendChild(countLabel);
    container.appendChild(toolbar);

    // Log area
    var logArea = document.createElement("div");
    logArea.style.cssText = "flex:1;overflow-y:auto;font-family:var(--font-mono);font-size:.78rem;line-height:1.5;padding:0;";
    container.appendChild(logArea);

    var entryCount = 0;

    function levelColor(level) {
      var l = level.toUpperCase();
      if (l === "ERROR") return "var(--error)";
      if (l === "WARN") return "var(--warn)";
      if (l === "DEBUG") return "var(--muted)";
      if (l === "TRACE") return "color-mix(in oklab, var(--muted) 60%, transparent)";
      return "var(--text)";
    }

    function levelBg(level) {
      var l = level.toUpperCase();
      if (l === "ERROR") return "rgba(239,68,68,0.08)";
      if (l === "WARN") return "rgba(245,158,11,0.06)";
      return "transparent";
    }

    function renderEntry(entry) {
      var row = document.createElement("div");
      row.style.cssText = "display:flex;gap:8px;padding:1px 16px;background:" + levelBg(entry.level) + ";border-bottom:1px solid var(--border);";

      var ts = document.createElement("span");
      ts.style.cssText = "color:var(--muted);flex-shrink:0;min-width:85px;";
      var d = new Date(entry.ts);
      ts.textContent = d.toLocaleTimeString([], { hour:"2-digit", minute:"2-digit", second:"2-digit" }) + "." + String(d.getMilliseconds()).padStart(3, "0");

      var lvl = document.createElement("span");
      lvl.style.cssText = "color:" + levelColor(entry.level) + ";flex-shrink:0;min-width:42px;font-weight:600;";
      lvl.textContent = entry.level.toUpperCase().substring(0, 5);

      var tgt = document.createElement("span");
      tgt.style.cssText = "color:var(--muted);flex-shrink:0;max-width:200px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;";
      tgt.textContent = entry.target;

      var msg = document.createElement("span");
      msg.style.cssText = "color:var(--text);white-space:pre-wrap;word-break:break-all;min-width:0;";
      msg.textContent = entry.message;

      // Append extra fields
      if (entry.fields && Object.keys(entry.fields).length > 0) {
        var extra = Object.keys(entry.fields).map(function (k) { return k + "=" + entry.fields[k]; }).join(" ");
        msg.textContent += " " + extra;
      }

      row.appendChild(ts);
      row.appendChild(lvl);
      row.appendChild(tgt);
      row.appendChild(msg);
      return row;
    }

    function appendEntry(entry) {
      var row = renderEntry(entry);
      logArea.appendChild(row);
      entryCount++;
      // Trim oldest entries if over limit
      while (logArea.childNodes.length > maxEntries) {
        logArea.removeChild(logArea.firstChild);
        entryCount--;
      }
      countLabel.textContent = entryCount + " entries";
      // Auto-scroll if near bottom
      if (!paused) {
        var atBottom = logArea.scrollHeight - logArea.scrollTop - logArea.clientHeight < 60;
        if (atBottom) logArea.scrollTop = logArea.scrollHeight;
      }
    }

    function matchesFilter(entry) {
      var minLevel = levelSelect.value;
      if (minLevel) {
        var levels = ["trace","debug","info","warn","error"];
        if (levels.indexOf(entry.level.toLowerCase()) < levels.indexOf(minLevel)) return false;
      }
      var tgtVal = targetInput.value.trim();
      if (tgtVal && entry.target.indexOf(tgtVal) === -1) return false;
      var searchVal = searchInput.value.trim().toLowerCase();
      if (searchVal && entry.message.toLowerCase().indexOf(searchVal) === -1 && entry.target.toLowerCase().indexOf(searchVal) === -1) return false;
      return true;
    }

    // Load initial entries
    sendRpc("logs.list", {
      level: levelSelect.value || undefined,
      target: targetInput.value.trim() || undefined,
      search: searchInput.value.trim() || undefined,
      limit: 500
    }).then(function (res) {
      if (!res || !res.ok) return;
      var entries = (res.payload && res.payload.entries) || [];
      entries.forEach(function (e) { appendEntry(e); });
      logArea.scrollTop = logArea.scrollHeight;
    });

    // Subscribe to live events
    logsEventHandler = function (entry) {
      if (paused) return;
      if (!matchesFilter(entry)) return;
      appendEntry(entry);
    };

    // Re-fetch when filters change
    function refetch() {
      logArea.textContent = "";
      entryCount = 0;
      sendRpc("logs.list", {
        level: levelSelect.value || undefined,
        target: targetInput.value.trim() || undefined,
        search: searchInput.value.trim() || undefined,
        limit: 500
      }).then(function (res) {
        if (!res || !res.ok) return;
        var entries = (res.payload && res.payload.entries) || [];
        entries.forEach(function (e) { appendEntry(e); });
        logArea.scrollTop = logArea.scrollHeight;
      });
    }

    levelSelect.addEventListener("change", refetch);
    var filterTimeout;
    function debouncedRefetch() {
      clearTimeout(filterTimeout);
      filterTimeout = setTimeout(refetch, 300);
    }
    targetInput.addEventListener("input", debouncedRefetch);
    searchInput.addEventListener("input", debouncedRefetch);

    pauseBtn.addEventListener("click", function () {
      paused = !paused;
      pauseBtn.textContent = paused ? "Resume" : "Pause";
      pauseBtn.style.borderColor = paused ? "var(--warn)" : "var(--border)";
      if (!paused) logArea.scrollTop = logArea.scrollHeight;
    });

    clearBtn.addEventListener("click", function () {
      logArea.textContent = "";
      entryCount = 0;
      countLabel.textContent = "0 entries";
    });
  }, function teardownLogs() {
    logsEventHandler = null;
  });

  // ── WebSocket ─────────────────────────────────────────────
  function connect() {
    setStatus("connecting", "connecting...");
    var proto = location.protocol === "https:" ? "wss:" : "ws:";
    ws = new WebSocket(proto + "//" + location.host + "/ws");

    ws.onopen = function () {
      var id = nextId();
      ws.send(JSON.stringify({
        type: "req", id: id, method: "connect",
        params: {
          minProtocol: 3, maxProtocol: 3,
          client: { id: "web-chat-ui", version: "0.1.0", platform: "browser", mode: "operator" }
        }
      }));
      pending[id] = function (frame) {
        var hello = frame.ok && frame.payload;
        if (hello && hello.type === "hello-ok") {
          connected = true;
          reconnectDelay = 1000;
          setStatus("connected", "connected (v" + hello.protocol + ")");
          var now = new Date();
          var ts = now.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
          chatAddMsg("system", "Connected to moltis gateway v" + hello.server.version + " at " + ts);
          fetchModels();
          fetchSessions();
          fetchProjects();
          // Fetch unseen log alerts from server
          sendRpc("logs.status", {}).then(function (res) {
            if (res && res.ok) {
              var p = res.payload || {};
              unseenErrors = p.unseen_errors || 0;
              unseenWarns = p.unseen_warns || 0;
              if (currentPage === "/logs") clearLogsAlert();
              else updateLogsAlert();
            }
          });
          // Re-mount the current page so it can fetch data now that we're connected
          mount(currentPage);
        } else {
          setStatus("", "handshake failed");
          var reason = (frame.error && frame.error.message) || "unknown error";
          chatAddMsg("error", "Handshake failed: " + reason);
        }
      };
    };

    ws.onmessage = function (evt) {
      var frame;
      try { frame = JSON.parse(evt.data); } catch (e) { return; }

      if (frame.type === "res") {
        var cb = pending[frame.id];
        if (cb) { delete pending[frame.id]; cb(frame); }
        return;
      }

      if (frame.type === "event") {
        var listeners = eventListeners[frame.event] || [];
        listeners.forEach(function(h) { h(frame.payload || {}); });
        if (frame.event === "chat") {
          var p = frame.payload || {};
          var eventSession = p.sessionKey || activeSessionKey;
          var isActive = eventSession === activeSessionKey;
          var isChatPage = currentPage === "/";

          // Refresh the session sidebar when we see a session key we don't know about yet.
          if (p.sessionKey && !sessions.find(function (s) { return s.key === p.sessionKey; })) {
            fetchSessions();
          }

          if (p.state === "thinking" && isActive && isChatPage) {
            removeThinking();
            var thinkEl = document.createElement("div");
            thinkEl.className = "msg assistant thinking";
            thinkEl.id = "thinkingIndicator";
            var thinkDots = document.createElement("span");
            thinkDots.className = "thinking-dots";
            // Safe: static hardcoded HTML, no user input
            thinkDots.innerHTML = "<span></span><span></span><span></span>";
            thinkEl.appendChild(thinkDots);
            chatMsgBox.appendChild(thinkEl);
            chatMsgBox.scrollTop = chatMsgBox.scrollHeight;
          } else if (p.state === "thinking_text" && isActive && isChatPage) {
            var indicator = document.getElementById("thinkingIndicator");
            if (indicator) {
              // Remove all children safely (dots or previous text)
              while (indicator.firstChild) indicator.removeChild(indicator.firstChild);
              var textEl = document.createElement("span");
              textEl.className = "thinking-text";
              textEl.textContent = p.text;
              indicator.appendChild(textEl);
              chatMsgBox.scrollTop = chatMsgBox.scrollHeight;
            }
          } else if (p.state === "thinking_done" && isActive && isChatPage) {
            removeThinking();
          } else if (p.state === "tool_call_start" && isActive && isChatPage) {
            removeThinking();
            var card = document.createElement("div");
            card.className = "msg exec-card running";
            card.id = "tool-" + p.toolCallId;
            var prompt = document.createElement("div");
            prompt.className = "exec-prompt";
            var cmd = (p.toolName === "exec" && p.arguments && p.arguments.command)
              ? p.arguments.command : (p.toolName || "tool");
            var promptChar = document.createElement("span");
            promptChar.className = "exec-prompt-char";
            promptChar.textContent = "$";
            prompt.appendChild(promptChar);
            var cmdSpan = document.createElement("span");
            cmdSpan.textContent = " " + cmd;
            prompt.appendChild(cmdSpan);
            card.appendChild(prompt);
            var spin = document.createElement("div");
            spin.className = "exec-status";
            spin.textContent = "running\u2026";
            card.appendChild(spin);
            chatMsgBox.appendChild(card);
            chatMsgBox.scrollTop = chatMsgBox.scrollHeight;
          } else if (p.state === "tool_call_end" && isActive && isChatPage) {
            var toolCard = document.getElementById("tool-" + p.toolCallId);
            if (toolCard) {
              toolCard.className = "msg exec-card " + (p.success ? "exec-ok" : "exec-err");
              var toolSpin = toolCard.querySelector(".exec-status");
              if (toolSpin) toolSpin.remove();
              if (p.success && p.result) {
                var out = (p.result.stdout || "").replace(/\n+$/, "");
                lastToolOutput = out;
                if (out) {
                  var outEl = document.createElement("pre");
                  outEl.className = "exec-output";
                  outEl.textContent = out;
                  toolCard.appendChild(outEl);
                }
                var stderrText = (p.result.stderr || "").replace(/\n+$/, "");
                if (stderrText) {
                  var errEl = document.createElement("pre");
                  errEl.className = "exec-output exec-stderr";
                  errEl.textContent = stderrText;
                  toolCard.appendChild(errEl);
                }
                if (p.result.exit_code !== undefined && p.result.exit_code !== 0) {
                  var codeEl = document.createElement("div");
                  codeEl.className = "exec-exit";
                  codeEl.textContent = "exit " + p.result.exit_code;
                  toolCard.appendChild(codeEl);
                }
              } else if (!p.success && p.error && p.error.detail) {
                var errMsg = document.createElement("div");
                errMsg.className = "exec-error-detail";
                errMsg.textContent = p.error.detail;
                toolCard.appendChild(errMsg);
              }
            }
          } else if (p.state === "channel_user" && isChatPage) {
            // Switch to the channel session if sessionKey is provided.
            if (p.sessionKey && p.sessionKey !== activeSessionKey) {
              switchSession(p.sessionKey);
            }
            var isActive = p.sessionKey ? (p.sessionKey === activeSessionKey) : isActive;
            if (!isActive) return;
            var cleanText = stripChannelPrefix(p.text || "");
            var el = chatAddMsg("user", renderMarkdown(cleanText), true);
            if (el && p.channel) {
              appendChannelFooter(el, p.channel);
            }
          } else if (p.state === "delta" && p.text && isActive && isChatPage) {
            removeThinking();
            if (!streamEl) {
              streamText = "";
              streamEl = document.createElement("div");
              streamEl.className = "msg assistant";
              chatMsgBox.appendChild(streamEl);
            }
            streamText += p.text;
            // Safe: renderMarkdown calls esc() first to escape all HTML entities,
            // then only adds our own formatting tags (pre, code, strong).
            streamEl.innerHTML = renderMarkdown(streamText);
            chatMsgBox.scrollTop = chatMsgBox.scrollHeight;
          } else if (p.state === "final") {
            bumpSessionCount(eventSession, 1);
            setSessionReplying(eventSession, false);
            if (!isActive) {
              setSessionUnread(eventSession, true);
            }
            if (isActive && isChatPage) {
              removeThinking();
              var isEcho = lastToolOutput && p.text
                && p.text.replace(/[`\s]/g, "").indexOf(lastToolOutput.replace(/\s/g, "").substring(0, 80)) !== -1;
              var msgEl = null;
              if (!isEcho) {
                if (p.text && streamEl) {
                  // Safe: renderMarkdown calls esc() first
                  streamEl.innerHTML = renderMarkdown(p.text);
                  msgEl = streamEl;
                } else if (p.text && !streamEl) {
                  msgEl = chatAddMsg("assistant", renderMarkdown(p.text), true);
                }
              } else if (streamEl) {
                streamEl.remove();
              }
              if (msgEl && p.model) {
                var footer = document.createElement("div");
                footer.className = "msg-model-footer";
                var footerText = p.provider ? p.provider + " / " + p.model : p.model;
                if (p.inputTokens || p.outputTokens) {
                  footerText += " \u00b7 " + formatTokens(p.inputTokens || 0) + " in / " + formatTokens(p.outputTokens || 0) + " out";
                }
                footer.textContent = footerText;
                msgEl.appendChild(footer);
              }
              // Accumulate session token totals.
              if (p.inputTokens || p.outputTokens) {
                sessionTokens.input += (p.inputTokens || 0);
                sessionTokens.output += (p.outputTokens || 0);
                updateTokenBar();
              }
              streamEl = null;
              streamText = "";
              lastToolOutput = "";
            }
          } else if (p.state === "auto_compact") {
            if (isActive && isChatPage) {
              if (p.phase === "start") {
                chatAddMsg("system", "Compacting conversation (context limit reached)\u2026");
              } else if (p.phase === "done") {
                // Remove the "Compacting..." message
                if (chatMsgBox && chatMsgBox.lastChild) chatMsgBox.removeChild(chatMsgBox.lastChild);
                renderCompactCard(p);
                // Reset session tokens since history was replaced
                sessionTokens = { input: 0, output: 0 };
                updateTokenBar();
              } else if (p.phase === "error") {
                if (chatMsgBox && chatMsgBox.lastChild) chatMsgBox.removeChild(chatMsgBox.lastChild);
                chatAddMsg("error", "Auto-compact failed: " + (p.error || "unknown error"));
              }
            }
          } else if (p.state === "error") {
            setSessionReplying(eventSession, false);
            if (isActive && isChatPage) {
              removeThinking();
              if (p.error && p.error.title) {
                chatAddErrorCard(p.error);
              } else {
                chatAddErrorMsg(p.message || "unknown");
              }
              streamEl = null;
              streamText = "";
            }
          }
        }
        if (frame.event === "exec.approval.requested") {
          var ap = frame.payload || {};
          renderApprovalCard(ap.requestId, ap.command);
        }
        if (frame.event === "logs.entry") {
          var logPayload = frame.payload || {};
          if (logsEventHandler) logsEventHandler(logPayload);
          if (currentPage !== "/logs") {
            var ll = (logPayload.level || "").toUpperCase();
            if (ll === "ERROR") { unseenErrors++; updateLogsAlert(); }
            else if (ll === "WARN") { unseenWarns++; updateLogsAlert(); }
          }
        }
        return;
      }
    };

    ws.onclose = function () {
      connected = false;
      setStatus("", "disconnected \u2014 reconnecting\u2026");
      streamEl = null;
      streamText = "";
      scheduleReconnect();
    };

    ws.onerror = function () {};
  }

  var reconnectTimer = null;

  function scheduleReconnect() {
    if (reconnectTimer) return;
    reconnectTimer = setTimeout(function () {
      reconnectTimer = null;
      reconnectDelay = Math.min(reconnectDelay * 1.5, 5000);
      connect();
    }, reconnectDelay);
  }

  document.addEventListener("visibilitychange", function () {
    if (!document.hidden && !connected) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
      reconnectDelay = 1000;
      connect();
    }
  });

  // ── Boot ──────────────────────────────────────────────────
  connect();
  mount(location.pathname);
})();
