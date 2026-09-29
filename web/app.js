const STAGES = [
  ["metadata", "Fetch"],
  ["download", "Download"],
  ["transcribe", "Transcribe"],
  ["extract", "Extract"],
  ["import", "Import"],
];
const STAGE_INDEX = Object.fromEntries(STAGES.map(([k], i) => [k, i]));
const STAGE_LABEL = Object.fromEntries(STAGES);
const STATUS_LABEL = {
  queued: "Queued",
  running: "Working",
  succeeded: "Imported",
  failed: "Failed",
  cancelled: "Cancelled",
};
const PLATFORM_EMOJI = { TikTok: "🎵", Instagram: "📸", YouTube: "▶️", Facebook: "📘", Pinterest: "📌" };

const state = {
  jobs: new Map(),
  filter: "all",
  config: {},
  tags: [],
  open: null,
  detail: null,
  panel: "activity",
};

const $ = (sel, root = document) => root.querySelector(sel);

const esc = (value) =>
  String(value ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

async function api(path, options = {}) {
  const res = await fetch(path, {
    ...options,
    headers: options.body ? { "Content-Type": "application/json" } : {},
  });
  if (res.status === 204 || res.status === 202) return null;
  const body = await res.json().catch(() => ({}));
  if (!res.ok) throw Object.assign(new Error(body.error || res.statusText), { status: res.status, body });
  return body;
}

// ── Formatting ─────────────────────────────────────────────────

function duration(ms) {
  if (ms == null || Number.isNaN(ms)) return "—";
  const s = ms / 1000;
  if (s < 1) return `${Math.round(ms)}ms`;
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${Math.round(s % 60)}s`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}

function ago(ms) {
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 45) return "just now";
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return new Date(ms).toLocaleDateString();
}

const clock = (ms) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });

function host(url) {
  try {
    return new URL(url).hostname.replace(/^www\./, "");
  } catch {
    return url;
  }
}

const mealieLink = (slug) => `${state.config.mealie_url}/g/${state.config.mealie_group}/r/${slug}`;
const jobTitle = (j) => j.recipe_name || j.title || host(j.url);

const cssUrl = (u) => u.replace(/["'()\\\s]/g, (c) => `%${c.charCodeAt(0).toString(16).padStart(2, "0")}`);

function thumb(j) {
  const src = /^https?:\/\//.test(j.thumbnail || "") ? j.thumbnail : null;
  const style = src ? ` style="background-image:url('${esc(cssUrl(src))}')"` : "";
  const emoji = src ? "" : PLATFORM_EMOJI[j.platform] || "🍽️";
  return `<div class="thumb"${style}>${emoji}</div>`;
}

// ── Stage stepper ──────────────────────────────────────────────

function stepState(j, key) {
  const idx = STAGE_INDEX[key];
  const skippable = key === "download" || key === "transcribe";
  const noAudio = j.media_kind && j.media_kind !== "video";
  const current = STAGE_INDEX[j.stage];
  if (j.status === "succeeded") return skippable && noAudio ? "skipped" : "done";
  if (j.status === "queued") return "pending";
  if (j.status === "failed" || j.status === "cancelled") {
    const at = STAGE_INDEX[j.error_stage ?? j.stage];
    if (at == null) return "pending";
    if (idx === at) return j.status === "failed" ? "error" : "pending";
    if (idx < at) return skippable && noAudio ? "skipped" : "done";
    return "pending";
  }
  if (current == null) return "pending";
  if (idx === current) return "active";
  if (idx < current) return skippable && noAudio ? "skipped" : "done";
  return skippable && noAudio && current > STAGE_INDEX.metadata ? "skipped" : "pending";
}

function stepper(j) {
  return `<div class="steps">${STAGES.map(([key, label]) => {
    const s = stepState(j, key);
    let track = "";
    if (s === "active") {
      track = j.progress == null ? `<div class="track indeterminate"><span></span></div>` : `<div class="track"><span style="width:${(j.progress * 100).toFixed(1)}%"></span></div>`;
    } else {
      track = `<div class="track"></div>`;
    }
    const pct = s === "active" && j.progress != null ? ` ${Math.round(j.progress * 100)}%` : "";
    return `<div class="step ${s}" title="${esc(label)}: ${s}">${track}<div class="name">${esc(label)}${pct}</div></div>`;
  }).join("")}</div>`;
}

// ── Queue list ─────────────────────────────────────────────────

function matchesFilter(j) {
  switch (state.filter) {
    case "active":
      return j.status === "queued" || j.status === "running";
    case "succeeded":
      return j.status === "succeeded";
    case "failed":
      return j.status === "failed" || j.status === "cancelled";
    default:
      return true;
  }
}

function jobActions(j) {
  const out = [];
  if (j.mealie_slug) out.push(`<a class="icon-btn" href="${esc(mealieLink(j.mealie_slug))}" target="_blank" rel="noopener" title="Open in Mealie" data-stop><svg class="icon"><use href="#i-external"/></svg></a>`);
  if (j.status === "failed" || j.status === "cancelled") out.push(`<button class="icon-btn" data-act="retry" title="Retry"><svg class="icon"><use href="#i-retry"/></svg></button>`);
  if (j.status === "queued" || j.status === "running") out.push(`<button class="icon-btn danger" data-act="cancel" title="Cancel"><svg class="icon"><use href="#i-stop"/></svg></button>`);
  else out.push(`<button class="icon-btn danger" data-act="delete" title="Remove from list"><svg class="icon"><use href="#i-trash"/></svg></button>`);
  return out.join("");
}

function jobMeta(j) {
  const parts = [j.platform || host(j.url)];
  if (j.uploader) parts.push(`@${j.uploader.replace(/^@/, "")}`);
  if (j.status === "running" && j.started_at) parts.push(`<span data-since="${j.started_at}">${duration(Date.now() - j.started_at)}</span>`);
  else if (j.finished_at && j.started_at && j.status === "succeeded") parts.push(`took ${duration(j.finished_at - j.started_at)}`);
  parts.push(`<span data-ago="${j.created_at}">${ago(j.created_at)}</span>`);
  if (j.tags?.length) parts.push(j.tags.map((t) => `#${esc(t)}`).join(" "));
  return parts.join(" · ");
}

function jobCard(j) {
  const error = j.status === "failed" && j.error ? `<div class="job-error">${esc(STAGE_LABEL[j.error_stage] || "")}${j.error_stage ? ": " : ""}${esc(j.error)}</div>` : "";
  const attempts = j.attempts > 1 ? ` · attempt ${j.attempts}` : "";
  return `<li class="job ${j.status}" data-id="${j.id}">
    ${thumb(j)}
    <div class="job-main">
      <div class="job-title">${esc(jobTitle(j))}</div>
      <div class="job-meta">${jobMeta(j)}${attempts}</div>
      ${j.status === "succeeded" ? "" : stepper(j)}
      ${error}
    </div>
    <div class="job-side">
      <span class="pill ${j.status}">${STATUS_LABEL[j.status] || j.status}</span>
      <div class="job-actions">${jobActions(j)}</div>
    </div>
  </li>`;
}

function renderJobs() {
  const all = [...state.jobs.values()].sort((a, b) => b.id - a.id);
  const shown = all.filter(matchesFilter);
  $("#jobs").innerHTML = shown.map(jobCard).join("");
  $("#empty").hidden = shown.length > 0;
  const active = all.filter((j) => j.status === "queued" || j.status === "running").length;
  const failed = all.filter((j) => j.status === "failed").length;
  $("#count-active").textContent = active || "";
  $("#count-failed").textContent = failed || "";
  const clearable = state.filter === "succeeded" || state.filter === "failed" || state.filter === "all";
  $("#clear").hidden = !clearable || !shown.some((j) => j.status !== "queued" && j.status !== "running");
  $("#clear").textContent = state.filter === "all" ? "Clear finished" : "Clear";
}

let renderQueued = false;
function scheduleRender() {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => {
    renderQueued = false;
    renderJobs();
  });
}

// ── Insights ───────────────────────────────────────────────────

async function loadStats() {
  try {
    const s = await api("/api/stats");
    $("#tiles").innerHTML = [
      ["In queue", s.queued, ""],
      ["Processing", s.running, ""],
      ["Imported", s.succeeded_7d, "ok"],
      ["Failed", s.failed_7d, s.failed_7d ? "bad" : ""],
    ]
      .map(([label, value, cls]) => `<div class="tile ${cls}"><div class="value">${value}</div><div class="label">${label}</div></div>`)
      .join("");

    const times = STAGES.map(([key, label]) => [label, s.stage_avg_ms.find((x) => x.stage === key)]).filter(([, v]) => v);
    const max = Math.max(1, ...times.map(([, v]) => v.avg_ms));
    $("#stage-times").innerHTML = times.length
      ? `<div class="section-label">Average time per stage</div><div class="bars">${times
          .map(([label, v]) => `<span>${label}</span><div class="bar"><span style="width:${(v.avg_ms / max) * 100}%"></span></div><span class="num">${duration(v.avg_ms)}</span>`)
          .join("")}</div>`
      : "";

    const fails = s.failures_by_stage;
    const fmax = Math.max(1, ...fails.map((f) => f.count));
    $("#failures").innerHTML =
      (fails.length
        ? `<div class="section-label">Where jobs fail</div><div class="bars">${fails
            .map((f) => `<span>${STAGE_LABEL[f.name] || f.name}</span><div class="bar bad"><span style="width:${(f.count / fmax) * 100}%"></span></div><span class="num">${f.count}</span>`)
            .join("")}</div>`
        : "") +
      `<div class="foot-note">${s.total_succeeded} recipes imported all-time · avg ${duration(s.avg_duration_ms)} per import · ${Number(s.tokens_7d).toLocaleString()} tokens this week</div>`;
  } catch (e) {
    console.warn("stats failed", e);
  }
}

let statsTimer = null;
function scheduleStats() {
  clearTimeout(statsTimer);
  statsTimer = setTimeout(loadStats, 800);
}

// ── Detail drawer ──────────────────────────────────────────────

function timeline(stages) {
  if (!stages.length) return `<p class="muted">Not started yet.</p>`;
  const attempts = [...new Set(stages.map((s) => s.attempt))];
  const now = Date.now();
  const spans = stages.map((s) => (s.finished_at ?? now) - s.started_at);
  const max = Math.max(1, ...spans);
  return `<div class="timeline">${attempts
    .map((a) => {
      const rows = stages
        .map((s, i) => [s, spans[i]])
        .filter(([s]) => s.attempt === a)
        .map(([s, span]) => `<span>${STAGE_LABEL[s.stage] || s.stage}</span><div class="bar ${s.outcome}" title="${s.outcome}"><span style="width:${Math.max(2, (span / max) * 100)}%"></span></div><span class="num">${s.outcome === "running" ? `<span data-since="${s.started_at}">${duration(span)}</span>` : duration(span)}</span>`)
        .join("");
      return (attempts.length > 1 ? `<div class="attempt-label">Attempt ${a}</div>` : "") + rows;
    })
    .join("")}</div>`;
}

function logLine(e) {
  return `<li class="${esc(e.level)}"><span class="t">${clock(e.at)}</span><span class="s">${esc(STAGE_LABEL[e.stage] || "")}</span><span class="m">${esc(e.message)}</span></li>`;
}

function recipeView(r) {
  if (!r) return `<p class="muted">The recipe appears here once the extract stage finishes.</p>`;
  const facts = [
    r.recipeYield && `🍽 ${r.recipeYield}`,
    r.prepTime && `Prep ${r.prepTime.replace(/^PT/, "").toLowerCase()}`,
    r.cookTime && `Cook ${r.cookTime.replace(/^PT/, "").toLowerCase()}`,
    r.totalTime && `Total ${r.totalTime.replace(/^PT/, "").toLowerCase()}`,
    ...(r.keywords || []).map((k) => `#${k}`),
  ].filter(Boolean);
  return `<div class="recipe">
    <h4>${esc(r.name)}</h4>
    <div class="muted">${esc(r.description)}</div>
    <div class="facts">${facts.map((f) => `<span class="tag">${esc(f)}</span>`).join("")}</div>
    <h5>Ingredients</h5>
    <ul>${(r.recipeIngredient || []).map((i) => `<li>${esc(i)}</li>`).join("")}</ul>
    <h5>Steps</h5>
    <ol>${(r.recipeInstructions || []).map((s) => `<li>${esc(s.text)}</li>`).join("")}</ol>
  </div>`;
}

function panelContent(d) {
  const j = d.job;
  switch (state.panel) {
    case "recipe":
      return recipeView(j.recipe_json);
    case "transcript":
      return j.transcript ? `<div class="prose">${esc(j.transcript)}</div>` : `<p class="muted">${j.media_kind && j.media_kind !== "video" ? "This post has no audio to transcribe." : "No transcript yet."}</p>`;
    case "caption":
      return j.description ? `<div class="prose">${esc(j.description)}</div>` : `<p class="muted">No caption found.</p>`;
    default:
      return d.events.length ? `<ol class="log" id="log">${d.events.map(logLine).join("")}</ol>` : `<p class="muted">No activity yet.</p>`;
  }
}

function renderDrawer() {
  const d = state.detail;
  if (!d) return;
  const j = { ...d.job, ...(state.jobs.get(d.job.id) || {}) };
  const actions = [];
  if (j.mealie_slug) actions.push(`<a class="btn small" href="${esc(mealieLink(j.mealie_slug))}" target="_blank" rel="noopener">Open in Mealie <svg class="icon"><use href="#i-external"/></svg></a>`);
  if (j.status === "failed" || j.status === "cancelled") {
    actions.push(`<button class="btn small" data-act="retry"><svg class="icon"><use href="#i-retry"/></svg> Retry</button>`);
    actions.push(`<button class="btn small ghost" data-act="retry-fresh" title="Discard cached transcript and recipe">Retry from scratch</button>`);
  }
  if (j.status === "succeeded") actions.push(`<button class="btn small ghost" data-act="retry-fresh">Re-import</button>`);
  if (j.status === "queued" || j.status === "running") actions.push(`<button class="btn small danger" data-act="cancel"><svg class="icon"><use href="#i-stop"/></svg> Cancel</button>`);
  else actions.push(`<button class="btn small ghost danger" data-act="delete"><svg class="icon"><use href="#i-trash"/></svg> Remove</button>`);

  const tokens = (j.prompt_tokens || 0) + (j.completion_tokens || 0);
  const meta = [
    ["Status", STATUS_LABEL[j.status]],
    ["Attempts", j.attempts],
    ["Platform", j.platform || "—"],
    ["Media", j.media_kind || "—"],
    ["Length", j.duration_secs ? duration(j.duration_secs * 1000) : "—"],
    ["Tokens", tokens ? tokens.toLocaleString() : "—"],
    ["Added", new Date(j.created_at).toLocaleString()],
    ["Total time", j.started_at && j.finished_at ? duration(j.finished_at - j.started_at) : "—"],
  ];

  const error = j.status === "failed" && j.error ? `<div class="callout bad"><strong>Failed during ${esc(STAGE_LABEL[j.error_stage] || j.error_stage || "processing")}</strong><pre>${esc(j.error)}</pre></div>` : "";
  const note = j.note ? `<div class="callout" style="background:var(--surface-2)"><strong>Notes for the AI:</strong> ${esc(j.note)}</div>` : "";
  const panels = [
    ["activity", "Activity"],
    ["recipe", "Recipe"],
    ["transcript", "Transcript"],
    ["caption", "Caption"],
  ];

  const logEl = $("#log");
  const stick = !logEl || logEl.scrollTop + logEl.clientHeight >= logEl.scrollHeight - 20;

  $("#drawer").innerHTML = `
    <div class="drawer-head">
      ${thumb(j)}
      <div>
        <h3>${esc(jobTitle(j))}</h3>
        <a class="source" href="${esc(j.url)}" target="_blank" rel="noopener">${esc(j.url)}</a>
        ${j.status === "succeeded" ? "" : stepper(j)}
      </div>
      <button class="icon-btn close" data-act="close" title="Close"><svg class="icon"><use href="#i-x"/></svg></button>
    </div>
    <div class="drawer-body">
      <div class="actions">${actions.join("")}</div>
      ${error}${note}
      <dl class="meta-grid">${meta.map(([k, v]) => `<div><dt>${k}</dt><dd>${esc(v)}</dd></div>`).join("")}</dl>
      <div><div class="section-label">Stage timeline</div>${timeline(d.stages)}</div>
      <div>
        <nav class="panel-tabs">${panels.map(([k, l]) => `<button data-panel="${k}" aria-selected="${state.panel === k}">${l}</button>`).join("")}</nav>
        <div style="margin-top:12px">${panelContent({ ...d, job: j })}</div>
      </div>
    </div>`;
  const newLog = $("#log");
  if (newLog && stick) newLog.scrollTop = newLog.scrollHeight;
}

async function loadDetail(id) {
  try {
    const d = await api(`/api/jobs/${id}`);
    if (state.open !== id) return;
    state.detail = d;
    renderDrawer();
  } catch (e) {
    if (e.status === 404) closeDrawer();
  }
}

let detailTimer = null;
function scheduleDetail() {
  clearTimeout(detailTimer);
  detailTimer = setTimeout(() => state.open && loadDetail(state.open), 300);
}

function openDrawer(id) {
  state.open = id;
  state.panel = "activity";
  state.detail = null;
  const job = state.jobs.get(id);
  if (job) {
    state.detail = { job, stages: [], events: [] };
    renderDrawer();
  }
  $("#drawer").classList.add("open");
  $("#drawer").setAttribute("aria-hidden", "false");
  $("#scrim").hidden = false;
  $("#drawer").focus();
  history.replaceState(null, "", `#job-${id}`);
  loadDetail(id);
}

function closeDrawer() {
  state.open = null;
  state.detail = null;
  $("#drawer").classList.remove("open");
  $("#drawer").setAttribute("aria-hidden", "true");
  $("#scrim").hidden = true;
  history.replaceState(null, "", location.pathname);
}

// ── Actions ────────────────────────────────────────────────────

function toast(message, { kind = "", action, href, timeout = 5000 } = {}) {
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.innerHTML = `<span class="grow">${esc(message)}</span>`;
  if (action) {
    const b = document.createElement("button");
    b.textContent = action.label;
    b.onclick = () => {
      el.remove();
      action.run();
    };
    el.append(b);
  }
  if (href) {
    const a = document.createElement("a");
    a.href = href.url;
    a.target = "_blank";
    a.rel = "noopener";
    a.textContent = href.label;
    el.append(a);
  }
  $("#toasts").append(el);
  setTimeout(() => el.remove(), timeout);
}

async function jobAction(id, act) {
  try {
    if (act === "retry") await api(`/api/jobs/${id}/retry`, { method: "POST", body: "{}" });
    if (act === "retry-fresh") await api(`/api/jobs/${id}/retry`, { method: "POST", body: JSON.stringify({ fresh: true }) });
    if (act === "cancel") await api(`/api/jobs/${id}/cancel`, { method: "POST" });
    if (act === "delete") {
      await api(`/api/jobs/${id}`, { method: "DELETE" });
      state.jobs.delete(id);
      if (state.open === id) closeDrawer();
      scheduleRender();
    }
    scheduleStats();
  } catch (e) {
    toast(e.message, { kind: "bad" });
  }
}

async function submitJob({ force = false } = {}) {
  const url = $("#url").value.trim();
  if (!url) return;
  const pending = $("#tag-input").value.trim();
  if (pending) addTag(pending);
  const btn = $("#submit-btn");
  btn.disabled = true;
  try {
    const job = await api("/api/jobs", {
      method: "POST",
      body: JSON.stringify({ url, tags: state.tags, note: $("#note").value, force }),
    });
    state.jobs.set(job.id, job);
    $("#url").value = "";
    $("#note").value = "";
    state.tags = [];
    renderChips();
    if (state.filter === "succeeded" || state.filter === "failed") setFilter("all");
    scheduleRender();
    scheduleStats();
    toast("Added to the queue", { action: { label: "View", run: () => openDrawer(job.id) } });
  } catch (e) {
    if (e.status === 409 && e.body?.existing) {
      const ex = e.body.existing;
      toast(`Already imported as “${jobTitle(ex)}”`, {
        timeout: 9000,
        href: ex.mealie_slug ? { url: mealieLink(ex.mealie_slug), label: "Open" } : undefined,
        action: { label: "Import again", run: () => submitJob({ force: true }) },
      });
    } else {
      toast(e.message, { kind: "bad" });
    }
  } finally {
    btn.disabled = false;
  }
}

function addTag(raw) {
  for (const t of raw.split(",").map((s) => s.trim()).filter(Boolean)) {
    if (!state.tags.some((x) => x.toLowerCase() === t.toLowerCase())) state.tags.push(t);
  }
  $("#tag-input").value = "";
  renderChips();
}

function renderChips() {
  const box = $("#chips");
  box.querySelectorAll(".chip").forEach((c) => c.remove());
  state.tags.forEach((t, i) => {
    const chip = document.createElement("span");
    chip.className = "chip";
    chip.innerHTML = `${esc(t)}<button type="button" aria-label="Remove ${esc(t)}">×</button>`;
    chip.querySelector("button").onclick = () => {
      state.tags.splice(i, 1);
      renderChips();
    };
    box.insertBefore(chip, $("#tag-input"));
  });
}

function setFilter(filter) {
  state.filter = filter;
  document.querySelectorAll("#tabs button").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.filter === filter)));
  renderJobs();
}

// ── Live updates ───────────────────────────────────────────────

async function loadJobs() {
  const jobs = await api("/api/jobs?limit=300");
  state.jobs = new Map(jobs.map((j) => [j.id, j]));
  renderJobs();
}

function setLive(mode) {
  const el = $("#live");
  el.className = `live ${mode}`;
  $(".live-label", el).textContent = { on: "Live", off: "Reconnecting", "": "Connecting" }[mode];
}

function connect() {
  const es = new EventSource("/api/events");
  es.onopen = () => {
    setLive("on");
    loadJobs().catch(() => {});
    loadStats();
    if (state.open) loadDetail(state.open);
  };
  es.onerror = () => setLive("off");
  es.onmessage = (msg) => {
    const u = JSON.parse(msg.data);
    if (u.type === "job") {
      const prev = state.jobs.get(u.job.id);
      state.jobs.set(u.job.id, u.job);
      scheduleRender();
      if (!prev || prev.status !== u.job.status) {
        scheduleStats();
        if (prev && u.job.status === "succeeded") {
          toast(`Imported “${jobTitle(u.job)}”`, { href: { url: mealieLink(u.job.mealie_slug), label: "Open" } });
        } else if (prev && u.job.status === "failed") {
          toast(`Failed: ${jobTitle(u.job)}`, { kind: "bad", action: { label: "Details", run: () => openDrawer(u.job.id) } });
        }
      }
      if (state.open === u.job.id) {
        if (!prev || prev.stage !== u.job.stage || prev.status !== u.job.status) scheduleDetail();
        else renderDrawer();
      }
    } else if (u.type === "log" && state.open === u.event.job_id && state.detail) {
      if (!state.detail.events.some((e) => e.id === u.event.id)) {
        state.detail.events.push(u.event);
        if (state.panel === "activity") {
          const log = $("#log");
          if (log) {
            const stick = log.scrollTop + log.clientHeight >= log.scrollHeight - 20;
            log.insertAdjacentHTML("beforeend", logLine(u.event));
            if (stick) log.scrollTop = log.scrollHeight;
          } else renderDrawer();
        }
      }
    } else if (u.type === "deleted") {
      state.jobs.delete(u.id);
      if (state.open === u.id) closeDrawer();
      scheduleRender();
      scheduleStats();
    } else if (u.type === "refresh") {
      loadJobs().catch(() => {});
      scheduleStats();
    }
  };
}

// ── Wiring ─────────────────────────────────────────────────────

$("#submit").addEventListener("submit", (e) => {
  e.preventDefault();
  submitJob();
});

$("#paste").addEventListener("click", async () => {
  try {
    $("#url").value = (await navigator.clipboard.readText()).trim();
    $("#url").focus();
  } catch {
    toast("Clipboard access was blocked — paste manually.");
  }
});

$("#tag-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter" || e.key === ",") {
    e.preventDefault();
    addTag(e.target.value);
  } else if (e.key === "Backspace" && !e.target.value && state.tags.length) {
    state.tags.pop();
    renderChips();
  }
});

$("#tabs").addEventListener("click", (e) => {
  const b = e.target.closest("button[data-filter]");
  if (b) setFilter(b.dataset.filter);
});

$("#clear").addEventListener("click", async () => {
  const status = state.filter === "all" ? "finished" : state.filter;
  const label = { finished: "all finished", succeeded: "all imported", failed: "all failed" }[status];
  if (!confirm(`Remove ${label} jobs from the list? Recipes stay in Mealie.`)) return;
  try {
    const { removed } = await api(`/api/jobs?status=${status}`, { method: "DELETE" });
    toast(`Removed ${removed} job${removed === 1 ? "" : "s"}`);
    await loadJobs();
    scheduleStats();
  } catch (e) {
    toast(e.message, { kind: "bad" });
  }
});

$("#jobs").addEventListener("click", (e) => {
  if (e.target.closest("[data-stop]")) return;
  const card = e.target.closest(".job");
  if (!card) return;
  const id = Number(card.dataset.id);
  const act = e.target.closest("[data-act]");
  if (act) {
    e.stopPropagation();
    jobAction(id, act.dataset.act);
  } else {
    openDrawer(id);
  }
});

$("#drawer").addEventListener("click", (e) => {
  const tab = e.target.closest("[data-panel]");
  if (tab) {
    state.panel = tab.dataset.panel;
    renderDrawer();
    return;
  }
  const act = e.target.closest("[data-act]");
  if (!act) return;
  if (act.dataset.act === "close") closeDrawer();
  else jobAction(state.open, act.dataset.act);
});

$("#scrim").addEventListener("click", closeDrawer);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && state.open) closeDrawer();
});

setInterval(() => {
  document.querySelectorAll("[data-since]").forEach((el) => (el.textContent = duration(Date.now() - Number(el.dataset.since))));
  document.querySelectorAll("[data-ago]").forEach((el) => (el.textContent = ago(Number(el.dataset.ago))));
}, 1000);

async function init() {
  state.config = await api("/api/config").catch(() => ({}));
  if (state.config.mealie_url) {
    const link = $("#mealie-link");
    link.href = state.config.mealie_url;
    link.hidden = false;
  }

  // Android share target delivers the link in url, text or title depending on the app.
  const params = new URLSearchParams(location.search);
  const shared = ["shared_url", "shared_text", "shared_title"].map((k) => params.get(k)).find((v) => v && /https?:\/\//.test(v));
  if (shared) {
    history.replaceState(null, "", location.pathname);
    $("#url").value = shared;
    submitJob();
  }

  await loadJobs().catch(() => toast("Could not load the queue", { kind: "bad" }));
  loadStats();
  connect();

  const m = location.hash.match(/^#job-(\d+)$/);
  if (m) openDrawer(Number(m[1]));
}

init();
