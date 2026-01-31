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
  var modelCombo = $("modelCombo");
  var modelComboBtn = $("modelComboBtn");
  var modelComboLabel = $("modelComboLabel");
  var modelDropdown = $("modelDropdown");
  var modelSearchInput = $("modelSearchInput");
  var modelDropdownList = $("modelDropdownList");
  var selectedModelId = localStorage.getItem("moltis-model") || "";
  var modelIdx = -1;
  function setSessionModel(sessionKey, modelId) {
    sendRpc("sessions.patch", { key: sessionKey, model: modelId });
  }
  var sessionsToggle = $("sessionsToggle");
  var sessionsPanel = $("sessionsPanel");
  var sessionList = $("sessionList");
  var newSessionBtn = $("newSessionBtn");
  var addProviderBtn = $("addProviderBtn");

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
      if (models.length === 0) {
        modelCombo.classList.add("hidden");
        return;
      }
      var saved = localStorage.getItem("moltis-model") || "";
      var found = models.find(function (m) { return m.id === saved; });
      if (found) {
        selectedModelId = found.id;
        modelComboLabel.textContent = found.displayName || found.id;
      } else {
        selectedModelId = models[0].id;
        modelComboLabel.textContent = models[0].displayName || models[0].id;
        localStorage.setItem("moltis-model", selectedModelId);
      }
      modelCombo.classList.remove("hidden");
    });
  }

  function selectModel(m) {
    selectedModelId = m.id;
    modelComboLabel.textContent = m.displayName || m.id;
    localStorage.setItem("moltis-model", m.id);
    setSessionModel(activeSessionKey, m.id);
    closeModelDropdown();
  }

  function openModelDropdown() {
    modelDropdown.classList.remove("hidden");
    modelSearchInput.value = "";
    modelIdx = -1;
    renderModelList("");
    requestAnimationFrame(function () { modelSearchInput.focus(); });
  }

  function closeModelDropdown() {
    modelDropdown.classList.add("hidden");
    modelSearchInput.value = "";
    modelIdx = -1;
  }

  function renderModelList(query) {
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
    var items = modelDropdownList.querySelectorAll(".model-dropdown-item");
    items.forEach(function (el, i) {
      el.classList.toggle("kb-active", i === modelIdx);
    });
    if (modelIdx >= 0 && items[modelIdx]) {
      items[modelIdx].scrollIntoView({ block: "nearest" });
    }
  }

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

  document.addEventListener("click", function (e) {
    if (!modelCombo.contains(e.target)) {
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

  // ── Sessions sidebar ────────────────────────────────────────
  sessionsToggle.addEventListener("click", function () {
    sessionsPanel.classList.toggle("hidden");
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
    sessions.forEach(function (s) {
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
      meta.textContent = count + " msg" + (count !== 1 ? "s" : "");
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
          if (confirm("Delete this session?")) {
            sendRpc("sessions.delete", { key: s.key }).then(function () {
              if (activeSessionKey === s.key) switchSession("main");
              fetchSessions();
            });
          }
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
    switchSession(key);
  });

  // ── Projects ──────────────────────────────────────────────────
  var projectSelect = $("projectSelect");

  function fetchProjects() {
    sendRpc("projects.list", {}).then(function (res) {
      if (!res || !res.ok) return;
      projects = res.payload || [];
      renderProjectSelect();
    });
  }

  function renderProjectSelect() {
    // Clear existing options safely
    while (projectSelect.firstChild) projectSelect.removeChild(projectSelect.firstChild);
    var defaultOpt = document.createElement("option");
    defaultOpt.value = "";
    defaultOpt.textContent = "No project";
    projectSelect.appendChild(defaultOpt);

    projects.forEach(function (p) {
      var opt = document.createElement("option");
      opt.value = p.id;
      opt.textContent = p.label || p.id;
      if (p.id === activeProjectId) opt.selected = true;
      projectSelect.appendChild(opt);
    });
  }

  projectSelect.addEventListener("change", function () {
    activeProjectId = projectSelect.value;
    localStorage.setItem("moltis-project", activeProjectId);
    // Persist project binding to the current session.
    if (connected && activeSessionKey) {
      sendRpc("sessions.switch", { key: activeSessionKey, project_id: activeProjectId });
    }
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
          setTimeout(closeProviderModal, 1500);
        }
      });
    }, 2000);
  }

  addProviderBtn.addEventListener("click", function () {
    if (connected) openProviderModal();
  });
  providerModalClose.addEventListener("click", closeProviderModal);
  providerModal.addEventListener("click", function (e) {
    if (e.target === providerModal) closeProviderModal();
  });

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
    chatMsgBox.scrollTop = chatMsgBox.scrollHeight;
    return el;
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

  function switchSession(key, searchContext) {
    activeSessionKey = key;
    localStorage.setItem("moltis-session", key);
    if (chatMsgBox) chatMsgBox.textContent = "";
    streamEl = null;
    streamText = "";
    sessionTokens = { input: 0, output: 0 };
    updateTokenBar();

    var items = sessionList.querySelectorAll(".session-item");
    items.forEach(function (el) {
      var isTarget = el.getAttribute("data-session-key") === key;
      el.classList.toggle("active", isTarget);
      if (isTarget) el.classList.remove("unread");
    });

    var switchParams = { key: key };
    if (activeProjectId) switchParams.project_id = activeProjectId;
    sendRpc("sessions.switch", switchParams).then(function (res) {
      if (res && res.ok && res.payload) {
        var entry = res.payload.entry || {};
        // Restore the session's project binding.
        if (entry.projectId) {
          activeProjectId = entry.projectId;
          localStorage.setItem("moltis-project", activeProjectId);
          projectSelect.value = activeProjectId;
        } else if (!switchParams.project_id) {
          // Session has no project — clear selection.
          activeProjectId = "";
          localStorage.setItem("moltis-project", "");
          projectSelect.value = "";
        }
        // Restore per-session model
        if (entry.model && models.length > 0) {
          var found = models.find(function (m) { return m.id === entry.model; });
          if (found) {
            selectedModelId = found.id;
            modelComboLabel.textContent = found.displayName || found.id;
            localStorage.setItem("moltis-model", found.id);
          }
        }
        var history = res.payload.history || [];
        var msgEls = [];
        history.forEach(function (msg) {
          if (msg.role === "user") {
            msgEls.push(chatAddMsg("user", renderMarkdown(msg.content || ""), true));
          } else if (msg.role === "assistant") {
            var el = chatAddMsg("assistant", renderMarkdown(msg.content || ""), true);
            if (el && msg.model) {
              var ft = document.createElement("div");
              ft.className = "msg-model-footer";
              ft.textContent = msg.provider ? msg.provider + " / " + msg.model : msg.model;
              el.appendChild(ft);
            }
            msgEls.push(el);
          } else {
            msgEls.push(null);
          }
        });

        if (searchContext && searchContext.query && chatMsgBox) {
          highlightAndScroll(msgEls, searchContext.messageIndex, searchContext.query);
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
          chatMsgBox.scrollTop = chatMsgBox.scrollHeight;
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

  function updateTokenBar() {
    var bar = $("tokenBar");
    if (!bar) return;
    var total = sessionTokens.input + sessionTokens.output;
    if (total === 0) {
      bar.textContent = "";
      return;
    }
    bar.textContent =
      formatTokens(sessionTokens.input) + " in / " +
      formatTokens(sessionTokens.output) + " out \u00b7 " +
      formatTokens(total) + " tokens";
  }

  // Safe: static hardcoded HTML template, no user input.
  var chatPageHTML =
    '<div class="flex-1 flex flex-col min-w-0">' +
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

    if (connected) chatSendBtn.disabled = false;

    chatInput.addEventListener("input", chatAutoResize);
    chatInput.addEventListener("keydown", function (e) {
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
    chatMsgBox = null;
    chatInput = null;
    chatSendBtn = null;
    streamEl = null;
    streamText = "";
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
      ["Name", "Schedule", "Enabled", "Next Run", "Last Status", "Actions"].forEach(function (h) {
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
          if (currentPage === "/") switchSession(activeSessionKey);
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
        if (frame.event === "chat") {
          var p = frame.payload || {};
          var eventSession = p.sessionKey || activeSessionKey;
          var isActive = eventSession === activeSessionKey;
          var isChatPage = currentPage === "/";

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
