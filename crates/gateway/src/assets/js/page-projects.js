// ── Projects page ────────────────────────────────────────
import * as S from "./state.js";
import { sendRpc, createEl } from "./helpers.js";
import { registerPage } from "./router.js";
import { fetchProjects } from "./projects.js";

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

  var listEl = createEl("div", { style: "max-width:600px;margin-top:8px;" });
  wrapper.appendChild(listEl);
  container.appendChild(wrapper);

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

  function renderList() {
    while (listEl.firstChild) listEl.removeChild(listEl.firstChild);
    if (S.projects.length === 0) {
      listEl.appendChild(createEl("div", {
        className: "text-xs text-[var(--muted)]",
        textContent: "No projects configured. Add a directory above or use auto-detect.",
        style: "padding:12px 0;"
      }));
      return;
    }
    S.projects.forEach(function (p) {
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

  // Fetch projects then render — needed for direct navigation
  sendRpc("projects.list", {}).then(function (res) {
    if (res && res.ok) {
      S.setProjects(res.payload || []);
    }
    renderList();
  });
});
