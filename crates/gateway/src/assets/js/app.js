// ── Entry point ────────────────────────────────────────────
"use strict";

import { initTheme, injectMarkdownStyles } from "./theme.js";
import { connect } from "./websocket.js";
import { onEvent } from "./events.js";
import { fetchSessions } from "./sessions.js";
import { mount } from "./router.js";

// Import page modules to register their routes
import "./page-chat.js";
import "./page-crons.js";
import "./page-projects.js";
import "./page-providers.js";
import "./page-channels.js";
import "./page-logs.js";
import "./page-skills.js";

// Import side-effect modules
import "./session-search.js";

initTheme();
injectMarkdownStyles();
onEvent("session", function () { fetchSessions(); });
mount(location.pathname);
connect();
