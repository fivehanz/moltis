// ── Session search ──────────────────────────────────────────
"use strict";

import * as S from "./state.js";
import { sendRpc, esc } from "./helpers.js";
import { navigate, currentPage } from "./router.js";
import { switchSession } from "./sessions.js";

var searchInput = S.$("sessionSearch");
var searchResults = S.$("searchResults");
searchResults.style.cssText = "position:absolute;left:0;right:0;top:100%;background:var(--surface);border:1px solid var(--border);border-radius:0 0 6px 6px;max-height:260px;overflow-y:auto;z-index:30;box-shadow:var(--shadow-md);";
var searchTimer = null;
var searchHits = [];
var searchIdx = -1;

function debounceSearch() {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(doSearch, 300);
}

function doSearch() {
  var q = searchInput.value.trim();
  if (!q || !S.connected) { hideSearch(); return; }
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
    el.style.cssText = "padding:8px 10px;cursor:pointer;border-bottom:1px solid var(--border);transition:background .1s;";
    el.setAttribute("data-idx", i);
    el.addEventListener("mouseenter", function () { el.style.background = "var(--bg-hover)"; });
    el.addEventListener("mouseleave", function () { el.style.background = ""; });

    var lbl = document.createElement("div");
    lbl.style.cssText = "font-size:.82rem;font-weight:500;color:var(--text);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;";
    lbl.textContent = hit.label || hit.sessionKey;
    el.appendChild(lbl);

    // Safe: esc() escapes all HTML entities first, then we only wrap
    // the already-escaped query substring in <mark> tags.
    var snip = document.createElement("div");
    snip.style.cssText = "font-size:.75rem;color:var(--muted);margin-top:2px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;";
    var escaped = esc(hit.snippet);
    var qEsc = esc(query);
    var re = new RegExp("(" + qEsc.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + ")", "gi");
    snip.innerHTML = escaped.replace(re, "<mark>$1</mark>");
    el.appendChild(snip);

    var role = document.createElement("div");
    role.style.cssText = "font-size:.68rem;color:var(--muted);margin-top:1px;opacity:.7;";
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
    el.style.background = (i === searchIdx) ? "var(--bg-hover)" : "";
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
