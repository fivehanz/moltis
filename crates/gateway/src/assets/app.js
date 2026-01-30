(function () {
  "use strict";

  var $ = function (id) { return document.getElementById(id); };
  var msgBox = $("messages");
  var input = $("chatInput");
  var sendBtn = $("sendBtn");
  var dot = $("statusDot");
  var sText = $("statusText");
  var methodsToggle = $("methodsToggle");
  var methodsPanel = $("methodsPanel");
  var rpcMethod = $("rpcMethod");
  var rpcParams = $("rpcParams");
  var rpcSend = $("rpcSend");
  var rpcResult = $("rpcResult");

  var modelSelect = $("modelSelect");

  var ws = null;
  var reqId = 0;
  var connected = false;
  var reconnectDelay = 1000;
  var streamEl = null;
  var streamText = "";
  var pending = {};
  var models = [];
  var lastToolOutput = "";

  // ── Theme ────────────────────────────────────────────────────────

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

  // ── Helpers ──────────────────────────────────────────────────────

  function nextId() { return "ui-" + (++reqId); }

  function setStatus(state, text) {
    dot.className = "status-dot " + state;
    sText.textContent = text;
    sendBtn.disabled = state !== "connected";
  }

  // Escape HTML entities to prevent XSS — all user/LLM text is escaped
  // before being processed by renderMarkdown, which produces safe HTML
  // from the already-escaped input.
  function esc(s) {
    return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
  }

  // Simple markdown: input is ALREADY HTML-escaped via esc(), so the
  // resulting HTML only contains tags we explicitly create.
  function renderMarkdown(raw) {
    var s = esc(raw);
    s = s.replace(/```(\w*)\n([\s\S]*?)```/g, function (_, lang, code) {
      return "<pre><code>" + code + "</code></pre>";
    });
    s = s.replace(/`([^`]+)`/g, "<code>$1</code>");
    s = s.replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>");
    return s;
  }

  // Sets element content. When isHtml is true the content MUST have
  // been produced by renderMarkdown (which escapes first).
  function addMsg(cls, content, isHtml) {
    var el = document.createElement("div");
    el.className = "msg " + cls;
    if (isHtml) {
      el.innerHTML = content; // safe: content is escaped via esc() then formatted
    } else {
      el.textContent = content;
    }
    msgBox.appendChild(el);
    msgBox.scrollTop = msgBox.scrollHeight;
    return el;
  }

  function removeThinking() {
    var el = document.getElementById("thinkingIndicator");
    if (el) el.remove();
  }

  function addErrorMsg(message) {
    var parsed = parseErrorMessage(message);
    var el = document.createElement("div");
    el.className = "msg error-card";

    var icon = document.createElement("div");
    icon.className = "error-icon";
    icon.textContent = parsed.icon;
    el.appendChild(icon);

    var body = document.createElement("div");
    body.className = "error-body";

    var title = document.createElement("div");
    title.className = "error-title";
    title.textContent = parsed.title;
    body.appendChild(title);

    if (parsed.detail) {
      var detail = document.createElement("div");
      detail.className = "error-detail";
      detail.textContent = parsed.detail;
      body.appendChild(detail);
    }

    if (parsed.resetsAt) {
      var countdown = document.createElement("div");
      countdown.className = "error-countdown";
      el.appendChild(body);
      el.appendChild(countdown);
      updateCountdown(countdown, parsed.resetsAt);
      var timer = setInterval(function () {
        var done = updateCountdown(countdown, parsed.resetsAt);
        if (done) clearInterval(timer);
      }, 1000);
    } else {
      el.appendChild(body);
    }

    msgBox.appendChild(el);
    msgBox.scrollTop = msgBox.scrollHeight;
  }

  function addErrorCard(err) {
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
        var done = updateCountdown(countdown, err.resetsAt);
        if (done) clearInterval(timer);
      }, 1000);
    } else {
      el.appendChild(body);
    }

    msgBox.appendChild(el);
    msgBox.scrollTop = msgBox.scrollHeight;
  }

  function parseErrorMessage(message) {
    // Try to extract JSON from the error message
    var jsonMatch = message.match(/\{[\s\S]*\}$/);
    if (jsonMatch) {
      try {
        var err = JSON.parse(jsonMatch[0]);
        var errObj = err.error || err;
        if (errObj.type === "usage_limit_reached" || (errObj.message && errObj.message.indexOf("usage limit") !== -1)) {
          return {
            icon: "",
            title: "Usage limit reached",
            detail: "Your " + (errObj.plan_type || "current") + " plan limit has been reached.",
            resetsAt: errObj.resets_at ? errObj.resets_at * 1000 : null
          };
        }
        if (errObj.type === "rate_limit_exceeded" || (errObj.message && errObj.message.indexOf("rate limit") !== -1)) {
          return {
            icon: "\u26A0\uFE0F",
            title: "Rate limited",
            detail: errObj.message || "Too many requests. Please wait a moment.",
            resetsAt: errObj.resets_at ? errObj.resets_at * 1000 : null
          };
        }
        if (errObj.message) {
          return { icon: "\u26A0\uFE0F", title: "Error", detail: errObj.message, resetsAt: null };
        }
      } catch (e) { /* fall through */ }
    }

    // Check for HTTP status codes in the raw message
    var statusMatch = message.match(/HTTP (\d{3})/);
    var code = statusMatch ? parseInt(statusMatch[1], 10) : 0;
    if (code === 401 || code === 403) {
      return { icon: "\uD83D\uDD12", title: "Authentication error", detail: "Your session may have expired. Try logging in again.", resetsAt: null };
    }
    if (code === 429) {
      return { icon: "", title: "Rate limited", detail: "Too many requests. Please wait a moment and try again.", resetsAt: null };
    }
    if (code >= 500) {
      return { icon: "\uD83D\uDEA8", title: "Server error", detail: "The upstream provider returned an error. Please try again later.", resetsAt: null };
    }

    return { icon: "\u26A0\uFE0F", title: "Error", detail: message, resetsAt: null };
  }

  function updateCountdown(el, resetsAtMs) {
    var now = Date.now();
    var diff = resetsAtMs - now;
    if (diff <= 0) {
      el.textContent = "Limit should be reset now — try again!";
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

  // ── WebSocket ────────────────────────────────────────────────────

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
          addMsg("system", "Connected to moltis gateway v" + hello.server.version + " at " + ts);
          fetchModels();
        } else {
          setStatus("", "handshake failed");
          var reason = (frame.error && frame.error.message) || "unknown error";
          addMsg("error", "Handshake failed: " + reason);
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
          if (p.state === "thinking") {
            removeThinking();
            var thinkEl = document.createElement("div");
            thinkEl.className = "msg assistant thinking";
            thinkEl.id = "thinkingIndicator";
            // Safe: static hardcoded content, no user input
            var dots = document.createElement("span");
            dots.className = "thinking-dots";
            dots.innerHTML = "<span></span><span></span><span></span>";
            thinkEl.appendChild(dots);
            msgBox.appendChild(thinkEl);
            msgBox.scrollTop = msgBox.scrollHeight;
          } else if (p.state === "thinking_done") {
            removeThinking();
          } else if (p.state === "tool_call_start") {
            removeThinking();
            var card = document.createElement("div");
            card.className = "msg exec-card running";
            card.id = "tool-" + p.toolCallId;
            var prompt = document.createElement("div");
            prompt.className = "exec-prompt";
            // For exec tool, show the shell command; otherwise show tool name
            var cmd = (p.toolName === "exec" && p.arguments && p.arguments.command)
              ? p.arguments.command
              : (p.toolName || "tool");
            var promptChar = document.createElement("span");
            promptChar.className = "exec-prompt-char";
            promptChar.textContent = "$";
            prompt.appendChild(promptChar);
            var cmdSpan = document.createElement("span");
            cmdSpan.textContent = " " + cmd;
            prompt.appendChild(cmdSpan);
            card.appendChild(prompt);
            // Spinner placeholder
            var spin = document.createElement("div");
            spin.className = "exec-status";
            spin.textContent = "running\u2026";
            card.appendChild(spin);
            msgBox.appendChild(card);
            msgBox.scrollTop = msgBox.scrollHeight;
          } else if (p.state === "tool_call_end") {
            var card = document.getElementById("tool-" + p.toolCallId);
            if (card) {
              card.className = "msg exec-card " + (p.success ? "exec-ok" : "exec-err");
              // Remove the spinner
              var spin = card.querySelector(".exec-status");
              if (spin) spin.remove();
              if (p.success && p.result) {
                // Show stdout output; also record it to suppress LLM echo
                var out = (p.result.stdout || "").replace(/\n+$/, "");
                lastToolOutput = out;
                if (out) {
                  var outEl = document.createElement("pre");
                  outEl.className = "exec-output";
                  outEl.textContent = out;
                  card.appendChild(outEl);
                }
                var err = (p.result.stderr || "").replace(/\n+$/, "");
                if (err) {
                  var errEl = document.createElement("pre");
                  errEl.className = "exec-output exec-stderr";
                  errEl.textContent = err;
                  card.appendChild(errEl);
                }
                if (p.result.exit_code !== undefined && p.result.exit_code !== 0) {
                  var code = document.createElement("div");
                  code.className = "exec-exit";
                  code.textContent = "exit " + p.result.exit_code;
                  card.appendChild(code);
                }
              } else if (!p.success && p.error && p.error.detail) {
                var errMsg = document.createElement("div");
                errMsg.className = "exec-error-detail";
                errMsg.textContent = p.error.detail;
                card.appendChild(errMsg);
              }
            }
          } else if (p.state === "delta" && p.text) {
            removeThinking();
            if (!streamEl) {
              streamText = "";
              streamEl = document.createElement("div");
              streamEl.className = "msg assistant";
              msgBox.appendChild(streamEl);
            }
            streamText += p.text;
            // Safe: renderMarkdown calls esc() first to escape all HTML entities,
            // then only adds our own formatting tags (pre, code, strong)
            streamEl.innerHTML = renderMarkdown(streamText);
            msgBox.scrollTop = msgBox.scrollHeight;
          } else if (p.state === "final") {
            removeThinking();
            // Suppress the LLM response when it just echoes tool output
            // already shown in the exec card.
            var isEcho = lastToolOutput && p.text
              && p.text.replace(/[`\s]/g, "").indexOf(lastToolOutput.replace(/\s/g, "").substring(0, 80)) !== -1;
            if (!isEcho) {
              if (p.text && streamEl) {
                // Safe: renderMarkdown escapes via esc() before formatting
                streamEl.innerHTML = renderMarkdown(p.text);
              } else if (p.text && !streamEl) {
                addMsg("assistant", renderMarkdown(p.text), true);
              }
            } else if (streamEl) {
              streamEl.remove();
            }
            streamEl = null;
            streamText = "";
            lastToolOutput = "";
          } else if (p.state === "error") {
            removeThinking();
            if (p.error && p.error.title) {
              addErrorCard(p.error);
            } else {
              // Backward compat: old payloads with just p.message
              addErrorMsg(p.message || "unknown");
            }
            streamEl = null;
            streamText = "";
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
      setStatus("", "disconnected — reconnecting…");
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

  // Reconnect immediately when the tab becomes visible again.
  document.addEventListener("visibilitychange", function () {
    if (!document.hidden && !connected) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
      reconnectDelay = 1000;
      connect();
    }
  });

  function fetchModels() {
    sendRpc("models.list", {}).then(function (res) {
      if (!res || !res.ok) return;
      models = res.payload || [];
      var saved = localStorage.getItem("moltis-model") || "";
      modelSelect.textContent = "";
      if (models.length === 0) {
        var opt = document.createElement("option");
        opt.value = "";
        opt.textContent = "no models";
        modelSelect.appendChild(opt);
        modelSelect.classList.add("hidden");
        return;
      }
      models.forEach(function (m) {
        var opt = document.createElement("option");
        opt.value = m.id;
        opt.textContent = m.displayName || m.id;
        if (m.id === saved) opt.selected = true;
        modelSelect.appendChild(opt);
      });
      modelSelect.classList.remove("hidden");
    });
  }

  modelSelect.addEventListener("change", function () {
    localStorage.setItem("moltis-model", modelSelect.value);
  });

  function sendRpc(method, params) {
    return new Promise(function (resolve) {
      var id = nextId();
      pending[id] = resolve;
      ws.send(JSON.stringify({ type: "req", id: id, method: method, params: params }));
    });
  }

  function sendChat() {
    var text = input.value.trim();
    if (!text || !connected) return;
    input.value = "";
    autoResize();
    addMsg("user", renderMarkdown(text), true);
    var chatParams = { text: text };
    var selectedModel = modelSelect.value;
    if (selectedModel) chatParams.model = selectedModel;
    sendRpc("chat.send", chatParams).then(function (res) {
      if (res && !res.ok && res.error) {
        addMsg("error", res.error.message || "Request failed");
      }
    });
  }

  function autoResize() {
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight, 120) + "px";
  }

  // ── Event listeners ──────────────────────────────────────────────

  input.addEventListener("input", autoResize);
  input.addEventListener("keydown", function (e) {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendChat(); }
  });
  sendBtn.addEventListener("click", sendChat);

  methodsToggle.addEventListener("click", function () {
    methodsPanel.classList.toggle("hidden");
  });

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

  // ── Approval cards ─────────────────────────────────────────────

  function renderApprovalCard(requestId, command) {
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

    // Countdown.
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

    msgBox.appendChild(card);
    msgBox.scrollTop = msgBox.scrollHeight;
  }

  function resolveApproval(requestId, decision, command, card) {
    var params = { requestId: requestId, decision: decision };
    if (command) params.command = command;
    sendRpc("exec.approval.resolve", params).then(function () {
      card.classList.add("approval-resolved");
      var btns = card.querySelectorAll(".approval-btn");
      btns.forEach(function (b) { b.disabled = true; });
      var status = document.createElement("div");
      status.className = "approval-status";
      status.textContent = decision === "approved" ? "Allowed" : "Denied";
      card.appendChild(status);
    });
  }

  // ── Provider modal ─────────────────────────────────────────────

  var providerModal = $("providerModal");
  var providerModalBody = $("providerModalBody");
  var providerModalTitle = $("providerModalTitle");
  var addProviderBtn = $("addProviderBtn");
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
          if (p.authType === "api-key") {
            showApiKeyForm(p);
          } else if (p.authType === "oauth") {
            showOAuthFlow(p);
          }
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

    var input = document.createElement("input");
    input.className = "provider-key-input";
    input.type = "password";
    input.placeholder = "sk-...";
    form.appendChild(input);

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
      var key = input.value.trim();
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
          input.style.borderColor = "var(--error)";
          label.textContent = err;
          label.style.color = "var(--error)";
        }
      });
    });
    btns.appendChild(saveBtn);
    form.appendChild(btns);

    providerModalBody.appendChild(form);
    input.focus();
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
    var maxAttempts = 60; // 60 * 2s = 120s
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

  connect();
  input.focus();
})();
