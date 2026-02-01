// ── Entry point ────────────────────────────────────────────

import { onEvent } from "./events.js";
import { renderSessionProjectSelect } from "./project-combo.js";
import { renderProjectSelect } from "./projects.js";
import { mount, navigate, registerPage } from "./router.js";
import { fetchSessions, renderSessionList } from "./sessions.js";
import * as S from "./state.js";
import { initTheme, injectMarkdownStyles } from "./theme.js";
import { connect } from "./websocket.js";

// Import page modules to register their routes
import "./page-chat.js";
import "./page-crons.js";
import "./page-projects.js";
import "./page-providers.js";
import "./page-channels.js";
import "./page-logs.js";
import "./page-plugins.js";
import "./page-skills.js";

// Import side-effect modules
import "./session-search.js";

// Redirect root to /chats
registerPage("/", () => {
	navigate("/chats");
});

initTheme();
injectMarkdownStyles();
onEvent("session", () => {
	fetchSessions();
});

// Mount the page immediately so the UI shell renders without waiting for data.
mount(location.pathname);
connect();

// Fetch bootstrap data asynchronously — populates sidebar, models, projects
// as soon as the data arrives, without blocking the initial page render.
fetch("/api/bootstrap")
	.then((r) => r.json())
	.then((boot) => {
		if (boot.channels) S.setCachedChannels(boot.channels.channels || boot.channels || []);
		if (boot.sessions) {
			S.setSessions(boot.sessions || []);
			renderSessionList();
		}
		if (boot.models) {
			S.setModels(boot.models || []);
			if (S.models.length > 0) {
				var saved = localStorage.getItem("moltis-model") || "";
				var found = S.models.find((m) => m.id === saved);
				if (found) {
					S.setSelectedModelId(found.id);
				} else {
					S.setSelectedModelId(S.models[0].id);
					localStorage.setItem("moltis-model", S.selectedModelId);
				}
			}
		}
		if (boot.projects) {
			S.setProjects(boot.projects || []);
			renderProjectSelect();
			renderSessionProjectSelect();
		}
	})
	.catch(() => {
		/* WS connect will fetch this data anyway */
	});
