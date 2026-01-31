// ── Chat page ────────────────────────────────────────────
import * as S from "./state.js";
import { sendRpc, renderMarkdown, formatTokens, formatBytes } from "./helpers.js";
import { registerPage } from "./router.js";
import { bindModelComboEvents } from "./models.js";
import { setSessionModel } from "./models.js";
import { bindSandboxToggleEvents, updateSandboxUI } from "./sandbox.js";
import { switchSession, bumpSessionCount, setSessionReplying } from "./sessions.js";
import { chatAddMsg, scrollChatToBottom, updateTokenBar } from "./chat-ui.js";

// ── Slash commands ───────────────────────────────────────
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

  var inputWrap = S.chatInput.parentElement;
  if (inputWrap && !slashMenuEl.parentElement) {
    inputWrap.classList.add("pos-relative");
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
  S.chatInput.value = "/" + slashMenuItems[idx].name;
  slashHideMenu();
  sendChat();
}

function slashHandleInput() {
  var val = S.chatInput.value;
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

// ── Context card helpers ─────────────────────────────────
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
  if (!S.chatMsgBox) return;
  slashInjectStyles();

  var card = ctxEl("div", "ctx-card");

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

  var sess = data.session || {};
  var sessSection = ctxSection("Session");
  sessSection.appendChild(ctxRow("Key", sess.key || "unknown", true));
  sessSection.appendChild(ctxRow("Messages", String(sess.messageCount || 0)));
  sessSection.appendChild(ctxRow("Model", sess.model || "default", true));
  if (sess.label) sessSection.appendChild(ctxRow("Label", sess.label));
  card.appendChild(sessSection);

  var proj = data.project;
  var projSection = ctxSection("Project");
  if (proj && proj !== null) {
    projSection.appendChild(ctxRow("Name", proj.label || "(unnamed)"));
    if (proj.directory) projSection.appendChild(ctxRow("Directory", proj.directory, true));
    if (proj.systemPrompt) projSection.appendChild(ctxRow("System Prompt", proj.systemPrompt.length + " chars"));
    var ctxFiles = proj.contextFiles || [];
    if (ctxFiles.length > 0) {
      var filesLabel = ctxEl("div", "ctx-section-title", "Context Files (" + ctxFiles.length + ")");
      filesLabel.classList.add("spaced");
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

  var tools = data.tools || [];
  var toolsSection = ctxSection("Tools");
  if (tools.length > 0) {
    var toolWrap = ctxEl("div", "");
    toolWrap.className = "ctx-tool-wrap";
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
  card.appendChild(tokenSection);

  S.chatMsgBox.appendChild(card);
  S.chatMsgBox.scrollTop = S.chatMsgBox.scrollHeight;
}

export function renderCompactCard(data) {
  if (!S.chatMsgBox) return;
  slashInjectStyles();

  var card = ctxEl("div", "ctx-card");

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

  var statsSection = ctxSection("Before compact");
  statsSection.appendChild(ctxRow("Messages", String(data.messageCount || 0)));
  statsSection.appendChild(ctxRow("Total tokens", formatTokens(data.totalTokens || 0)));
  if (data.contextWindow) {
    var pctUsed = Math.round((data.totalTokens || 0) / data.contextWindow * 100);
    statsSection.appendChild(ctxRow("Context usage", pctUsed + "% of " + formatTokens(data.contextWindow)));
  }
  card.appendChild(statsSection);

  var afterSection = ctxSection("After compact");
  afterSection.appendChild(ctxRow("Messages", "1 (summary)"));
  afterSection.appendChild(ctxRow("Status", "Conversation history replaced with a summary"));
  card.appendChild(afterSection);

  S.chatMsgBox.appendChild(card);
  S.chatMsgBox.scrollTop = S.chatMsgBox.scrollHeight;
}

// ── Send chat message ────────────────────────────────────
function sendChat() {
  var text = S.chatInput.value.trim();
  if (!text || !S.connected) return;

  if (text.charAt(0) === "/") {
    var cmdName = text.substring(1).toLowerCase();
    var matched = slashCommands.find(function (c) { return c.name === cmdName; });
    if (matched) {
      S.chatInput.value = "";
      chatAutoResize();
      slashHideMenu();
      if (cmdName === "clear") {
        sendRpc("chat.clear", {}).then(function (res) {
          if (res && res.ok) {
            if (S.chatMsgBox) S.chatMsgBox.textContent = "";
            S.setSessionTokens({ input: 0, output: 0 });
            updateTokenBar();
            bumpSessionCount(S.activeSessionKey, 0);
          } else {
            chatAddMsg("error", (res && res.error && res.error.message) || "Clear failed");
          }
        });
      } else if (cmdName === "compact") {
        chatAddMsg("system", "Compacting conversation\u2026");
        sendRpc("chat.compact", {}).then(function (res) {
          if (res && res.ok) {
            switchSession(S.activeSessionKey);
          } else {
            chatAddMsg("error", (res && res.error && res.error.message) || "Compact failed");
          }
        });
      } else if (cmdName === "context") {
        chatAddMsg("system", "Loading context\u2026");
        sendRpc("chat.context", {}).then(function (res) {
          if (S.chatMsgBox && S.chatMsgBox.lastChild) S.chatMsgBox.removeChild(S.chatMsgBox.lastChild);
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

  S.chatHistory.push(text);
  if (S.chatHistory.length > 200) S.setChatHistory(S.chatHistory.slice(-200));
  localStorage.setItem("moltis-chat-history", JSON.stringify(S.chatHistory));
  S.setChatHistoryIdx(-1);
  S.setChatHistoryDraft("");
  S.chatInput.value = "";
  chatAutoResize();
  chatAddMsg("user", renderMarkdown(text), true);
  var chatParams = { text: text };
  var selectedModel = S.selectedModelId;
  if (selectedModel) {
    chatParams.model = selectedModel;
    setSessionModel(S.activeSessionKey, selectedModel);
  }
  bumpSessionCount(S.activeSessionKey, 1);
  setSessionReplying(S.activeSessionKey, true);
  sendRpc("chat.send", chatParams).then(function (res) {
    if (res && !res.ok && res.error) {
      chatAddMsg("error", res.error.message || "Request failed");
    }
  });
}

function chatAutoResize() {
  if (!S.chatInput) return;
  S.chatInput.style.height = "auto";
  S.chatInput.style.height = Math.min(S.chatInput.scrollHeight, 120) + "px";
}

// Safe: static hardcoded HTML template string — no user input is interpolated.
var chatPageHTML =
  '<div class="flex-1 flex flex-col min-w-0">' +
    '<div class="px-4 py-1.5 border-b border-[var(--border)] bg-[var(--surface)] flex items-center gap-2 shrink-0">' +
      '<div id="modelCombo" class="model-combo">' +
        '<button id="modelComboBtn" class="model-combo-btn" type="button">' +
          '<span id="modelComboLabel">loading\u2026</span>' +
          '<svg class="model-combo-chevron" xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="2" stroke="currentColor" width="12" height="12"><path d="M19.5 8.25l-7.5 7.5-7.5-7.5"/></svg>' +
        '</button>' +
        '<div id="modelDropdown" class="model-dropdown hidden">' +
          '<input id="modelSearchInput" type="text" placeholder="Search models\u2026" class="model-search-input" autocomplete="off" />' +
          '<div id="modelDropdownList" class="model-dropdown-list"></div>' +
        '</div>' +
      '</div>' +
      '<button id="sandboxToggle" class="sandbox-toggle text-xs border border-[var(--border)] px-2 py-1 rounded-md transition-colors cursor-pointer bg-transparent font-[var(--font-body)]" style="display:inline-flex;align-items:center;gap:4px;" title="Toggle sandbox mode">' +
        '<svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" width="14" height="14" style="flex-shrink:0;"><path d="M16.5 10.5V6.75a4.5 4.5 0 1 0-9 0v3.75m-.75 11.25h10.5a2.25 2.25 0 0 0 2.25-2.25v-6.75a2.25 2.25 0 0 0-2.25-2.25H6.75a2.25 2.25 0 0 0-2.25 2.25v6.75a2.25 2.25 0 0 0 2.25 2.25Z"/></svg>' +
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
  // Safe: chatPageHTML is a static hardcoded template with no user input
  container.innerHTML = chatPageHTML;

  S.setChatMsgBox(S.$("messages"));
  S.setChatInput(S.$("chatInput"));
  S.setChatSendBtn(S.$("sendBtn"));

  S.setModelCombo(S.$("modelCombo"));
  S.setModelComboBtn(S.$("modelComboBtn"));
  S.setModelComboLabel(S.$("modelComboLabel"));
  S.setModelDropdown(S.$("modelDropdown"));
  S.setModelSearchInput(S.$("modelSearchInput"));
  S.setModelDropdownList(S.$("modelDropdownList"));
  bindModelComboEvents();

  S.setSandboxToggleBtn(S.$("sandboxToggle"));
  S.setSandboxLabel(S.$("sandboxLabel"));
  bindSandboxToggleEvents();
  updateSandboxUI(true);

  if (S.models.length > 0 && S.modelComboLabel) {
    var found = S.models.find(function (m) { return m.id === S.selectedModelId; });
    if (found) {
      S.modelComboLabel.textContent = found.displayName || found.id;
    } else if (S.models[0]) {
      S.modelComboLabel.textContent = S.models[0].displayName || S.models[0].id;
    }
  }

  if (S.connected) {
    S.chatSendBtn.disabled = false;
    switchSession(S.activeSessionKey);
  }

  S.chatInput.addEventListener("input", function () { chatAutoResize(); slashHandleInput(); });
  S.chatInput.addEventListener("keydown", function (e) {
    if (slashHandleKeydown(e)) return;
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendChat(); return; }
    if (e.key === "ArrowUp" && S.chatInput.selectionStart === 0 && !e.shiftKey) {
      if (S.chatHistory.length === 0) return;
      e.preventDefault();
      if (S.chatHistoryIdx === -1) {
        S.setChatHistoryDraft(S.chatInput.value);
        S.setChatHistoryIdx(S.chatHistory.length - 1);
      } else if (S.chatHistoryIdx > 0) {
        S.setChatHistoryIdx(S.chatHistoryIdx - 1);
      }
      S.chatInput.value = S.chatHistory[S.chatHistoryIdx];
      chatAutoResize();
      return;
    }
    if (e.key === "ArrowDown" && S.chatInput.selectionStart === S.chatInput.value.length && !e.shiftKey) {
      if (S.chatHistoryIdx === -1) return;
      e.preventDefault();
      if (S.chatHistoryIdx < S.chatHistory.length - 1) {
        S.setChatHistoryIdx(S.chatHistoryIdx + 1);
        S.chatInput.value = S.chatHistory[S.chatHistoryIdx];
      } else {
        S.setChatHistoryIdx(-1);
        S.chatInput.value = S.chatHistoryDraft;
      }
      chatAutoResize();
      return;
    }
  });
  S.chatSendBtn.addEventListener("click", sendChat);

  if (S.connected) switchSession(S.activeSessionKey);
  S.chatInput.focus();
}, function teardownChat() {
  slashHideMenu();
  S.setChatMsgBox(null);
  S.setChatInput(null);
  S.setChatSendBtn(null);
  S.setStreamEl(null);
  S.setStreamText("");
  S.setModelCombo(null);
  S.setModelComboBtn(null);
  S.setModelComboLabel(null);
  S.setModelDropdown(null);
  S.setModelSearchInput(null);
  S.setModelDropdownList(null);
  S.setSandboxToggleBtn(null);
  S.setSandboxLabel(null);
});
