//! The single-page, dependency-free read-only observability dashboard.
//!
//! [`DASHBOARD_HTML`] is a self-contained page (inline `<style>` + vanilla JS,
//! no CDN) that fetches `/workflows`, lets you drill into a workflow's runs via
//! `/workflows/{id}/runs`, and inspect a run via `/runs/{run_id}`. It is served
//! verbatim at `GET /` and must contain the literal text "A2W".

/// The full HTML document for the dashboard. Served as-is.
pub const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>A2W Observability</title>
<style>
  :root { color-scheme: light dark; }
  * { box-sizing: border-box; }
  body {
    margin: 0;
    font: 14px/1.5 system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
    color: #1b1f24;
    background: #f6f8fa;
  }
  header {
    background: #0d1117;
    color: #fff;
    padding: 14px 20px;
    display: flex;
    align-items: baseline;
    gap: 12px;
  }
  header h1 { margin: 0; font-size: 18px; letter-spacing: 0.5px; }
  header .sub { color: #9aa4b2; font-size: 12px; }
  main {
    display: grid;
    grid-template-columns: 280px 280px 1fr;
    gap: 16px;
    padding: 16px 20px;
    align-items: start;
  }
  section {
    background: #fff;
    border: 1px solid #d0d7de;
    border-radius: 8px;
    overflow: hidden;
  }
  section > h2 {
    margin: 0;
    padding: 10px 12px;
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.6px;
    color: #57606a;
    border-bottom: 1px solid #eaeef2;
    background: #f6f8fa;
  }
  ul { list-style: none; margin: 0; padding: 0; max-height: 60vh; overflow: auto; }
  li {
    padding: 8px 12px;
    border-bottom: 1px solid #eaeef2;
    cursor: pointer;
  }
  li:hover { background: #f0f6ff; }
  li.active { background: #ddf4ff; }
  li .id { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; }
  li .name { color: #57606a; font-size: 12px; }
  .muted { padding: 10px 12px; color: #8c959f; font-size: 12px; }
  pre {
    margin: 0;
    padding: 12px;
    font: 12px/1.45 ui-monospace, SFMono-Regular, Menlo, monospace;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 70vh;
    overflow: auto;
  }
  .err { color: #cf222e; }
  @media (prefers-color-scheme: dark) {
    body { color: #e6edf3; background: #0d1117; }
    section { background: #161b22; border-color: #30363d; }
    section > h2 { background: #0d1117; color: #8b949e; border-color: #21262d; }
    li { border-color: #21262d; }
    li:hover { background: #1c2430; }
    li.active { background: #143049; }
    .muted { color: #6e7681; }
  }
</style>
</head>
<body>
<header>
  <h1>A2W</h1>
  <span class="sub">read-only workflow &amp; run observability</span>
</header>
<main>
  <section>
    <h2>Workflows</h2>
    <ul id="workflows"><li class="muted">loading&hellip;</li></ul>
  </section>
  <section>
    <h2>Runs</h2>
    <ul id="runs"><li class="muted">select a workflow</li></ul>
  </section>
  <section>
    <h2>Detail</h2>
    <pre id="detail">select a run to inspect its persisted record</pre>
  </section>
</main>
<script>
(function () {
  "use strict";
  var workflowsEl = document.getElementById("workflows");
  var runsEl = document.getElementById("runs");
  var detailEl = document.getElementById("detail");

  function clear(el) { while (el.firstChild) el.removeChild(el.firstChild); }

  function muted(el, text) {
    clear(el);
    var li = document.createElement("li");
    li.className = "muted";
    li.textContent = text;
    el.appendChild(li);
  }

  function getJSON(url) {
    return fetch(url).then(function (r) {
      if (!r.ok) throw new Error("HTTP " + r.status + " for " + url);
      return r.json();
    });
  }

  function showDetail(obj) {
    detailEl.classList.remove("err");
    detailEl.textContent = JSON.stringify(obj, null, 2);
  }

  function showError(e) {
    detailEl.classList.add("err");
    detailEl.textContent = String(e && e.message ? e.message : e);
  }

  function selectWorkflow(id, li) {
    Array.prototype.forEach.call(workflowsEl.children, function (c) {
      c.classList.remove("active");
    });
    if (li) li.classList.add("active");
    muted(runsEl, "loading runs…");
    getJSON("/workflows/" + encodeURIComponent(id) + "/runs")
      .then(function (runIds) {
        clear(runsEl);
        if (!runIds.length) { muted(runsEl, "no runs yet"); return; }
        runIds.forEach(function (runId) {
          var r = document.createElement("li");
          var s = document.createElement("span");
          s.className = "id";
          s.textContent = runId;
          r.appendChild(s);
          r.addEventListener("click", function () { selectRun(runId, r); });
          runsEl.appendChild(r);
        });
      })
      .catch(showError);
  }

  function selectRun(runId, li) {
    Array.prototype.forEach.call(runsEl.children, function (c) {
      c.classList.remove("active");
    });
    if (li) li.classList.add("active");
    getJSON("/runs/" + encodeURIComponent(runId)).then(showDetail).catch(showError);
  }

  function loadWorkflows() {
    getJSON("/workflows")
      .then(function (list) {
        clear(workflowsEl);
        if (!list.length) { muted(workflowsEl, "no workflows stored"); return; }
        list.forEach(function (wf) {
          var li = document.createElement("li");
          var id = document.createElement("div");
          id.className = "id";
          id.textContent = wf.id;
          var name = document.createElement("div");
          name.className = "name";
          name.textContent = wf.name;
          li.appendChild(id);
          li.appendChild(name);
          li.addEventListener("click", function () { selectWorkflow(wf.id, li); });
          workflowsEl.appendChild(li);
        });
      })
      .catch(function (e) { muted(workflowsEl, "failed to load: " + e.message); });
  }

  loadWorkflows();
})();
</script>
</body>
</html>
"#;
