// ── Crons page (Preact + HTM + Signals) ──────────────────────

import { signal } from "@preact/signals";
import { html } from "htm/preact";
import { render } from "preact";
import { useEffect } from "preact/hooks";
import * as gon from "./gon.js";
import { sendRpc } from "./helpers.js";
import { updateNavCount } from "./nav-counts.js";
import { registerPage } from "./router.js";
import * as S from "./state.js";
import { ConfirmDialog, Modal, requestConfirm } from "./ui.js";

var initialCrons = gon.get("crons") || [];
var cronJobs = signal(initialCrons);
var cronStatus = signal(gon.get("cron_status"));
if (initialCrons.length) {
	updateNavCount("crons", initialCrons.filter((j) => j.enabled).length);
}
var runsHistory = signal(null); // { jobId, jobName, runs }
var showModal = signal(false);
var editingJob = signal(null);

function loadStatus() {
	sendRpc("cron.status", {}).then((res) => {
		if (res?.ok) cronStatus.value = res.payload;
	});
}

function loadJobs() {
	sendRpc("cron.list", {}).then((res) => {
		if (res?.ok) {
			cronJobs.value = res.payload || [];
			updateNavCount("crons", cronJobs.value.filter((j) => j.enabled).length);
		}
	});
}

function formatSchedule(sched) {
	if (sched.kind === "at") return `At ${new Date(sched.atMs).toLocaleString()}`;
	if (sched.kind === "every") {
		var ms = sched.everyMs;
		if (ms >= 3600000) return `Every ${ms / 3600000}h`;
		if (ms >= 60000) return `Every ${ms / 60000}m`;
		return `Every ${ms / 1000}s`;
	}
	if (sched.kind === "cron") return sched.expr + (sched.tz ? ` (${sched.tz})` : "");
	return JSON.stringify(sched);
}

function StatusBar() {
	var s = cronStatus.value;
	if (!s) return html`<div class="cron-status-bar">Loading\u2026</div>`;
	var parts = [
		s.running ? "Running" : "Stopped",
		`${s.jobCount} job${s.jobCount !== 1 ? "s" : ""}`,
		`${s.enabledCount} enabled`,
	];
	if (s.nextRunAtMs) {
		parts.push(`next: ${new Date(s.nextRunAtMs).toLocaleString()}`);
	}
	return html`<div class="cron-status-bar">${parts.join(" \u2022 ")}</div>`;
}

function CronJobRow(props) {
	var job = props.job;

	function onToggle(e) {
		sendRpc("cron.update", {
			id: job.id,
			patch: { enabled: e.target.checked },
		}).then(() => {
			loadJobs();
			loadStatus();
		});
	}

	function onRun() {
		sendRpc("cron.run", { id: job.id, force: true }).then(() => {
			loadJobs();
			loadStatus();
		});
	}

	function onDelete() {
		requestConfirm(`Delete job '${job.name}'?`).then((yes) => {
			if (!yes) return;
			sendRpc("cron.remove", { id: job.id }).then(() => {
				loadJobs();
				loadStatus();
			});
		});
	}

	function onHistory() {
		runsHistory.value = { jobId: job.id, jobName: job.name, runs: null };
		sendRpc("cron.runs", { id: job.id }).then((res) => {
			if (res?.ok)
				runsHistory.value = {
					jobId: job.id,
					jobName: job.name,
					runs: res.payload || [],
				};
		});
	}

	return html`<tr>
    <td>${job.name}</td>
    <td class="cron-mono">${formatSchedule(job.schedule)}</td>
    <td class="cron-mono">${job.state?.nextRunAtMs ? html`<time data-epoch-ms="${job.state.nextRunAtMs}">${new Date(job.state.nextRunAtMs).toISOString()}</time>` : "\u2014"}</td>
    <td>${job.state?.lastStatus ? html`<span class="cron-badge ${job.state.lastStatus}">${job.state.lastStatus}</span>` : "\u2014"}</td>
    <td class="cron-actions">
      <button class="cron-action-btn" onClick=${() => {
				editingJob.value = job;
				showModal.value = true;
			}}>Edit</button>
      <button class="cron-action-btn" onClick=${onRun}>Run</button>
      <button class="cron-action-btn" onClick=${onHistory}>History</button>
      <button class="cron-action-btn cron-action-danger" onClick=${onDelete}>Delete</button>
    </td>
    <td>
      <label class="cron-toggle">
        <input type="checkbox" checked=${job.enabled} onChange=${onToggle} />
        <span class="cron-slider" />
      </label>
    </td>
  </tr>`;
}

function CronJobTable() {
	var jobs = cronJobs.value;
	if (jobs.length === 0) {
		return html`<div class="text-sm text-[var(--muted)]">No cron jobs configured.</div>`;
	}
	return html`<table class="cron-table">
    <thead>
      <tr>
        <th>Name</th><th>Schedule</th>
        <th>Next Run</th><th>Last Status</th><th>Actions</th><th>Enabled</th>
      </tr>
    </thead>
    <tbody>
      ${jobs.map((job) => html`<${CronJobRow} key=${job.id} job=${job} />`)}
    </tbody>
  </table>`;
}

function RunHistoryPanel() {
	var h = runsHistory.value;
	if (!h) return null;

	return html`<div class="mb-md">
    <div class="flex items-center justify-between mb-md">
      <span class="text-sm font-medium text-[var(--text-strong)]">Run History: ${h.jobName}</span>
      <button class="text-xs text-[var(--muted)] cursor-pointer bg-transparent border-none hover:text-[var(--text)]"
        onClick=${() => {
					runsHistory.value = null;
				}}>\u2715 Close</button>
    </div>
    ${h.runs === null && html`<div class="text-sm text-[var(--muted)]">Loading\u2026</div>`}
    ${h.runs !== null && h.runs.length === 0 && html`<div class="text-xs text-[var(--muted)]">No runs yet.</div>`}
    ${h.runs?.map(
			(run) => html`<div class="cron-run-item" key=${run.startedAtMs}>
        <span class="text-xs text-[var(--muted)]"><time data-epoch-ms="${run.startedAtMs}">${new Date(run.startedAtMs).toISOString()}</time></span>
        <span class="cron-badge ${run.status}">${run.status}</span>
        <span class="text-xs text-[var(--muted)]">${run.durationMs}ms</span>
        ${run.error && html`<span class="text-xs text-[var(--error)]">${run.error}</span>`}
      </div>`,
		)}
  </div>`;
}

function parseScheduleFromForm(form, kind) {
	if (kind === "at") {
		var ts = new Date(form.querySelector("[data-field=at]").value).getTime();
		if (Number.isNaN(ts)) return { error: "at" };
		return { schedule: { kind: "at", atMs: ts } };
	}
	if (kind === "every") {
		var secs = parseInt(form.querySelector("[data-field=every]").value, 10);
		if (Number.isNaN(secs) || secs <= 0) return { error: "every" };
		return { schedule: { kind: "every", everyMs: secs * 1000 } };
	}
	var expr = form.querySelector("[data-field=cron]").value.trim();
	if (!expr) return { error: "cron" };
	var schedule = { kind: "cron", expr: expr };
	var tz = form.querySelector("[data-field=tz]").value.trim();
	if (tz) schedule.tz = tz;
	return { schedule: schedule };
}

function schedDefault(kind, job) {
	if (!job) return "";
	if (kind === "at" && job.schedule.kind === "at" && job.schedule.atMs) {
		return new Date(job.schedule.atMs).toISOString().slice(0, 16);
	}
	if (kind === "every" && job.schedule.kind === "every" && job.schedule.everyMs) {
		return Math.round(job.schedule.everyMs / 1000);
	}
	return "";
}

function CronModal() {
	var isEdit = !!editingJob.value;
	var job = editingJob.value;
	var saving = signal(false);
	var schedKind = signal(isEdit ? job.schedule.kind : "cron");
	var errorField = signal(null);

	function onSave(e) {
		e.preventDefault();
		var form = e.target.closest(".provider-key-form");
		var name = form.querySelector("[data-field=name]").value.trim();
		if (!name) {
			errorField.value = "name";
			return;
		}
		var parsed = parseScheduleFromForm(form, schedKind.value);
		if (parsed.error) {
			errorField.value = parsed.error;
			return;
		}
		var msgText = form.querySelector("[data-field=message]").value.trim();
		if (!msgText) {
			errorField.value = "message";
			return;
		}
		var payloadKind = form.querySelector("[data-field=payloadKind]").value;
		var payload =
			payloadKind === "systemEvent"
				? { kind: "systemEvent", text: msgText }
				: { kind: "agentTurn", message: msgText, deliver: false };
		var fields = {
			name: name,
			schedule: parsed.schedule,
			payload: payload,
			sessionTarget: form.querySelector("[data-field=target]").value,
			deleteAfterRun: form.querySelector("[data-field=deleteAfter]").checked,
			enabled: form.querySelector("[data-field=enabled]").checked,
		};

		saving.value = true;
		var rpcMethod = isEdit ? "cron.update" : "cron.add";
		var rpcParams = isEdit ? { id: job.id, patch: fields } : fields;
		sendRpc(rpcMethod, rpcParams).then((res) => {
			saving.value = false;
			if (res?.ok) {
				showModal.value = false;
				editingJob.value = null;
				loadJobs();
				loadStatus();
			}
		});
	}

	function schedParams() {
		if (schedKind.value === "at") {
			return html`<input data-field="at" class="provider-key-input" type="datetime-local"
        value=${schedDefault("at", job)} />`;
		}
		if (schedKind.value === "every") {
			return html`<input data-field="every" class="provider-key-input" type="number" placeholder="Interval in seconds" min="1"
        value=${schedDefault("every", job)} />`;
		}
		return html`
      <input data-field="cron" class="provider-key-input" placeholder="*/5 * * * *"
        value=${isEdit && job.schedule.kind === "cron" ? job.schedule.expr || "" : ""} />
      <input data-field="tz" class="provider-key-input" placeholder="Timezone (optional, e.g. Europe/Paris)"
        value=${isEdit && job.schedule.kind === "cron" ? job.schedule.tz || "" : ""} />
    `;
	}

	return html`<${Modal} show=${showModal.value} onClose=${() => {
		showModal.value = false;
		editingJob.value = null;
	}} title=${isEdit ? "Edit Job" : "Add Job"}>
    <div class="provider-key-form">
      <label class="text-xs text-[var(--muted)]">Name</label>
      <input data-field="name" class="provider-key-input ${errorField.value === "name" ? "field-error" : ""}"
        placeholder="Job name" value=${isEdit ? job.name : ""} />

      <label class="text-xs text-[var(--muted)]">Schedule Type</label>
      <select data-field="schedKind" class="provider-key-input" value=${schedKind.value}
        onChange=${(e) => {
					schedKind.value = e.target.value;
				}}>
        <option value="at">At (one-shot)</option>
        <option value="every">Every (interval)</option>
        <option value="cron">Cron (expression)</option>
      </select>

      ${schedParams()}

      <label class="text-xs text-[var(--muted)]">Payload Type</label>
      <select data-field="payloadKind" class="provider-key-input"
        value=${isEdit ? job.payload.kind : "systemEvent"}>
        <option value="systemEvent">System Event</option>
        <option value="agentTurn">Agent Turn</option>
      </select>

      <label class="text-xs text-[var(--muted)]">Message</label>
      <textarea data-field="message" class="provider-key-input textarea-sm ${errorField.value === "message" ? "field-error" : ""}"
        placeholder="Message text">${isEdit ? job.payload.text || job.payload.message || "" : ""}</textarea>

      <label class="text-xs text-[var(--muted)]">Session Target</label>
      <select data-field="target" class="provider-key-input"
        value=${isEdit ? job.sessionTarget || "isolated" : "isolated"}>
        <option value="isolated">Isolated</option>
        <option value="main">Main</option>
      </select>

      <label class="text-xs text-[var(--muted)] flex items-center gap-2">
        <input data-field="deleteAfter" type="checkbox" checked=${isEdit ? job.deleteAfterRun : false} />
        Delete after run
      </label>
      <label class="text-xs text-[var(--muted)] flex items-center gap-2">
        <input data-field="enabled" type="checkbox" checked=${isEdit ? job.enabled : true} />
        Enabled
      </label>

      <div class="btn-row-mt">
        <button class="provider-btn provider-btn-secondary" onClick=${() => {
					showModal.value = false;
					editingJob.value = null;
				}}>Cancel</button>
        <button class="provider-btn" onClick=${onSave} disabled=${saving.value}>
          ${saving.value ? "Saving\u2026" : isEdit ? "Update" : "Create"}
        </button>
      </div>
    </div>
  </${Modal}>`;
}

function CronsPage() {
	useEffect(() => {
		loadStatus();
		loadJobs();
	}, []);

	return html`
    <div class="flex-1 flex flex-col min-w-0 p-4 gap-4 overflow-y-auto">
      <div class="flex items-center gap-3">
        <h2 class="text-lg font-medium text-[var(--text-strong)]">Cron Jobs</h2>
        <button class="bg-[var(--accent-dim)] text-white border-none px-3 py-1.5 rounded text-xs cursor-pointer hover:bg-[var(--accent)] transition-colors"
          onClick=${() => {
						editingJob.value = null;
						showModal.value = true;
					}}>+ Add Job</button>
        <button class="text-xs text-[var(--muted)] border border-[var(--border)] px-2.5 py-1 rounded-md hover:text-[var(--text)] hover:border-[var(--border-strong)] transition-colors cursor-pointer bg-transparent"
          onClick=${() => {
						loadJobs();
						loadStatus();
					}}>Refresh</button>
      </div>
      <${StatusBar} />
      <${CronJobTable} />
      <${RunHistoryPanel} />
    </div>
    <${CronModal} />
    <${ConfirmDialog} />
  `;
}

registerPage(
	"/crons",
	function initCrons(container) {
		container.style.cssText = "flex-direction:column;padding:0;overflow:hidden;";
		cronJobs.value = gon.get("crons") || [];
		cronStatus.value = gon.get("cron_status");
		runsHistory.value = null;
		showModal.value = false;
		editingJob.value = null;
		render(html`<${CronsPage} />`, container);
	},
	function teardownCrons() {
		var container = S.$("pageContent");
		if (container) render(null, container);
	},
);
