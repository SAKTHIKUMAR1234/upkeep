"use strict";

const REFRESH_MS = 3000;
const ROLE_NOTES = { viewer: "sees only", operator: "can start updates", admin: "can add users" };
const STEP_ICONS = {
  ok: ["✓", "ok"], degraded: ["⚠", "warn"], failed: ["✗", "bad"], running: ["⏳", ""],
  reboot: ["↻", "warn"], skipped: ["–", "muted"], pending: ["·", "muted"],
};
const state = { me: null, hosts: [], plans: {}, timer: null, confirming: null, job: null };
const $ = (id) => document.getElementById(id);

/** An element with a class and text or child nodes. Text is never parsed as HTML. */
function el(tag, className, ...children) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  node.append(...children.filter((child) => child !== null && child !== undefined));
  return node;
}

function button(text, className, onClick) {
  const node = el("button", className, text);
  node.type = "button";
  node.addEventListener("click", onClick);
  return node;
}

async function api(path, { method = "GET", body } = {}) {
  const headers = { Accept: "application/json" };
  if (body !== undefined) headers["Content-Type"] = "application/json";
  if (method !== "GET") headers["X-CSRF-Token"] = state.me ? state.me.csrf : "";
  const response = await fetch("/api" + path, {
    method, headers, credentials: "same-origin",
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const isJson = (response.headers.get("content-type") || "").includes("json");
  const data = isJson ? await response.json() : await response.text();
  if (response.status === 401) {
    showSignIn();
    throw new Error("Please sign in again.");
  }
  if (!response.ok) throw new Error((data && data.error) || response.statusText);
  return data;
}

const pad = (number) => String(number).padStart(2, "0");

/** For example "16-11 23:55", in the browser's time zone. */
function when(unix) {
  if (!unix) return "";
  const date = new Date(unix * 1000);
  return `${pad(date.getDate())}-${pad(date.getMonth() + 1)} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function minutes(from, to) {
  return Math.max(1, Math.round((to - from) / 60));
}

const isActive = (status) => status === "running" || status === "rebooting";
const canOperate = () => state.me && state.me.role !== "viewer";
const isAdmin = () => state.me && state.me.role === "admin";

/** The result of a job in words, with its color class. */
function result(job) {
  const took = job.finished_at ? `, ${minutes(job.created_at, job.finished_at)} min` : "";
  switch (job.status) {
    case "succeeded": return ["ok", `Done${took}`];
    case "degraded": return ["warn", `Done with warnings${took}`];
    case "failed": return ["bad", job.phase === "recovery" ? "Failed, recovery ran" : "Failed"];
    case "cancelled": return ["muted", "Stopped"];
    case "rebooting": return ["warn", "Restarting the server"];
    default: return ["warn", job.cancel_requested ? "Stopping after this step" : "Running"];
  }
}

const resultIcon = (job) => ({ ok: "✅", bad: "❌", warn: "⏳", muted: "⏹" })[result(job)[0]];

function planTitle(hostName, target, fallback = target) {
  const plan = (state.plans[hostName] || []).find((one) => one.name === target);
  return plan ? plan.title : fallback;
}

function hostLabel(hostName) {
  const host = state.hosts.find((one) => one.name === hostName);
  return host ? host.label : hostName;
}

function showMessage(text) {
  $("message").textContent = text;
  $("message").hidden = !text;
}

function show(section) {
  for (const id of ["sign-in", "servers", "progress", "history"]) $(id).hidden = id !== section;
  $("bar").hidden = section === "sign-in";
  showMessage("");
}

function stopRefresh() {
  clearTimeout(state.timer);
  state.timer = null;
}

function showSignIn() {
  stopRefresh();
  state.me = null;
  show("sign-in");
}

async function loadHosts() {
  state.hosts = (await api("/hosts")).hosts;
  const lists = await Promise.all(state.hosts.map((host) => host.reachable
    ? api(`/hosts/${encodeURIComponent(host.name)}/plans`).then((body) => body.plans).catch(() => [])
    : []));
  state.hosts.forEach((host, index) => { state.plans[host.name] = lists[index]; });
}

/* Servers */

function healthBadge(host) {
  if (!host.reachable) return el("span", "bad", "❌ Not reachable");
  const monitor = host.monitor;
  if (monitor.critical > 0) return el("span", "bad", `❌ ${monitor.critical + monitor.warn}`);
  if (monitor.warn > 0) return el("span", "warn", `⚠️ ${monitor.warn}`);
  return el("span", "ok", "✅ All OK");
}

function checksLine(monitor) {
  if (monitor.enabled === false) return "Checks are off on this server";
  const parts = [`${monitor.ok} checks OK`, ...monitor.problems.map((problem) => problem.title)];
  return parts.join(" · ");
}

function updatesLine(updates) {
  if (!updates || !updates.packages) return null;
  const risky = updates.risky ? `, ${updates.risky} need care` : "";
  return `${updates.packages} OS updates waiting${risky}`;
}

function latestJobLine(host) {
  const job = host.latest_job;
  if (!job || !job.id) return el("div", "muted", "No updates yet");
  const line = el("div", "row");
  const text = `Last update: ${planTitle(host.name, job.target)}, ${when(job.created_at)} ${resultIcon(job)}`;
  line.append(el("span", "muted", text));
  const label = isActive(job.status) ? "Show progress" : "Details";
  line.append(button(label, "btn ghost", () => { location.hash = `#job/${host.name}/${job.id}`; }));
  return line;
}

function planRow(host, plan) {
  const row = el("div", "row", el("span", "", plan.title));
  if (plan.error) {
    row.append(el("span", "bad", "Plan has an error"));
    row.title = plan.error;
  } else if (canOperate()) {
    row.append(button("Update", "btn", () => openConfirm(host, plan)));
  }
  return row;
}

function card(host) {
  const node = el("div", "card", el("div", "row", el("b", "", host.label), healthBadge(host)));
  if (!host.reachable) {
    node.append(el("div", "muted", host.error));
    return node;
  }
  node.append(el("div", "muted", checksLine(host.monitor)));
  const updates = updatesLine(host.monitor.updates);
  if (updates) node.append(el("div", "muted", updates));
  node.append(latestJobLine(host));
  for (const plan of state.plans[host.name] || []) node.append(planRow(host, plan));
  return node;
}

async function showServers() {
  show("servers");
  $("viewer-note").hidden = canOperate();
  $("cards").replaceChildren(el("p", "muted", "Loading servers…"));
  await loadHosts();
  $("cards").replaceChildren(...state.hosts.map(card));
}

/* Confirm dialog */

function openConfirm(host, plan) {
  state.confirming = { host, plan };
  $("confirm-title").textContent = `Start "${plan.title}" on ${host.label}?`;
  $("confirm-facts").textContent = updatesLine(host.monitor && host.monitor.updates) || "";
  $("confirm-steps").textContent = `Steps: ${plan.steps.join(" → ")}`;
  const recovery = plan.on_failure || [];
  $("confirm-recovery").textContent = recovery.length
    ? `If a step fails, Upkeep runs: ${recovery.join(" → ")}` : "";
  $("confirm-error").textContent = "";
  $("confirm-start").disabled = false;
  $("confirm-dialog").showModal();
}

async function confirmStart() {
  const { host, plan } = state.confirming;
  $("confirm-start").disabled = true;
  try {
    const path = `/hosts/${encodeURIComponent(host.name)}/run`;
    const job = await api(path, { method: "POST", body: { plan: plan.name } });
    $("confirm-dialog").close();
    location.hash = `#job/${host.name}/${job.id}`;
  } catch (error) {
    $("confirm-error").textContent = error.message;
    $("confirm-start").disabled = false;
  }
}

/* Live progress */

function stepLine(step, job) {
  const [icon, className] = STEP_ICONS[step.status] || ["?", ""];
  const text = el("span", "", step.title || `${step.adapter}: ${step.action}`);
  if (step.status === "running" && step.started_at) {
    const now = Math.floor(Date.now() / 1000);
    text.append(" ", el("span", "muted", `(${minutes(step.started_at, now)} min)`));
  }
  if (step.status === "reboot" && isActive(job.status)) text.append(" ", el("span", "muted", "(server restarting)"));
  const line = el("div", `step ${step.status === "pending" ? "pending" : ""}`, el("span", `icon ${className}`, icon), text);
  if (step.error) line.append(el("span", "bad", step.error));
  return line;
}

function renderJob(hostName, detail) {
  const { job, steps } = detail;
  $("job-title").textContent = `${planTitle(hostName, job.target, detail.title)} · ${hostLabel(hostName)}`;
  $("job-who").textContent = `started by ${job.requested_by} at ${when(job.created_at)}`;
  const main = steps.filter((step) => step.phase === "main");
  const recovery = steps.filter((step) => step.phase === "recovery");
  const lines = main.map((step) => stepLine(step, job));
  if (job.phase === "recovery" && recovery.length) {
    lines.push(el("p", "muted", "A step failed, so Upkeep runs the recovery steps:"));
    lines.push(...recovery.map((step) => stepLine(step, job)));
  }
  $("job-steps").replaceChildren(...lines);
  const [className, text] = result(job);
  $("job-result").className = className;
  const stepFailed = steps.some((step) => step.status === "failed");
  $("job-result").textContent = job.error && !stepFailed ? `${text}: ${job.error}` : text;
  $("job-cancel").hidden = !(canOperate() && isActive(job.status) && !job.cancel_requested);
}

async function showJob(hostName, id) {
  show("progress");
  $("job-log").hidden = true;
  $("job-log-toggle").textContent = "Show log";
  if (!state.hosts.length) await loadHosts();
  const expected = location.hash;
  state.job = { hostName, id };
  const refresh = async () => {
    const detail = await api(`/hosts/${encodeURIComponent(hostName)}/jobs/${id}`);
    if (location.hash !== expected) return;
    renderJob(hostName, detail);
    if (!$("job-log").hidden) await loadLog();
    if (isActive(detail.job.status)) state.timer = setTimeout(() => refresh().catch(fail), REFRESH_MS);
  };
  await refresh();
}

async function loadLog() {
  const { hostName, id } = state.job;
  const log = $("job-log");
  const nearEnd = log.scrollTop + log.clientHeight >= log.scrollHeight - 20;
  log.textContent = await api(`/hosts/${encodeURIComponent(hostName)}/jobs/${id}/log`) || "The log is empty.";
  if (nearEnd) log.scrollTop = log.scrollHeight;
}

async function toggleLog() {
  const log = $("job-log");
  log.hidden = !log.hidden;
  $("job-log-toggle").textContent = log.hidden ? "Show log" : "Hide log";
  if (!log.hidden) await loadLog();
}

async function cancelJob() {
  const { hostName, id } = state.job;
  if (!confirm("Stop this update after the current step?")) return;
  await api(`/hosts/${encodeURIComponent(hostName)}/jobs/${id}/cancel`, { method: "POST", body: {} });
  $("job-cancel").hidden = true;
}

/* History and users */

function jobRow(hostName, job) {
  const [className, text] = result(job);
  const row = el("tr", "clickable",
    el("td", "", when(job.created_at)), el("td", "", hostLabel(hostName)),
    el("td", "", planTitle(hostName, job.target)), el("td", "", job.requested_by),
    el("td", className, text));
  row.addEventListener("click", () => { location.hash = `#job/${hostName}/${job.id}`; });
  return row;
}

async function loadJobs() {
  const reachable = state.hosts.filter((host) => host.reachable);
  const lists = await Promise.all(reachable.map((host) =>
    api(`/hosts/${encodeURIComponent(host.name)}/jobs?limit=50`)
      .then((body) => body.jobs.map((job) => [host.name, job])).catch(() => [])));
  const rows = lists.flat().sort((left, right) => right[1].created_at - left[1].created_at);
  $("jobs").replaceChildren(...rows.map(([hostName, job]) => jobRow(hostName, job)));
  if (!rows.length) $("jobs").append(el("tr", "", el("td", "muted", "No updates yet")));
}

function userRow(user) {
  const remove = user.email === state.me.email ? el("span", "muted", "· this is you")
    : button("Remove", "link", () => removeUser(user.email).catch(fail));
  return el("tr", "",
    el("td", "", user.email), el("td", "", el("span", "chip", user.role)),
    el("td", "", el("span", "muted", ROLE_NOTES[user.role]), " ", remove));
}

function auditRow(entry) {
  return el("tr", "", el("td", "", when(entry.at)), el("td", "", entry.email),
    el("td", "", entry.action), el("td", "", entry.host ? hostLabel(entry.host) : ""),
    el("td", "", entry.detail));
}

async function loadAdmin() {
  const [users, audit] = await Promise.all([api("/users"), api("/audit")]);
  $("users").replaceChildren(...users.users.map(userRow));
  $("audit").replaceChildren(...audit.audit.map(auditRow));
}

async function addUser(event) {
  event.preventDefault();
  const body = { email: $("new-email").value.trim(), role: $("new-role").value };
  await api("/users", { method: "POST", body });
  $("new-email").value = "";
  await loadAdmin();
}

async function removeUser(email) {
  if (!confirm(`Remove ${email}? They are signed out at once.`)) return;
  await api("/users", { method: "DELETE", body: { email } });
  await loadAdmin();
}

async function showHistory() {
  show("history");
  $("admin").hidden = !isAdmin();
  $("jobs").replaceChildren(el("tr", "", el("td", "muted", "Loading…")));
  if (!state.hosts.length) await loadHosts();
  await Promise.all([loadJobs(), isAdmin() ? loadAdmin() : null]);
}

/* Start */

function fail(error) {
  if (state.me) showMessage(error.message);
}

function route() {
  stopRefresh();
  if (!state.me) return;
  const job = location.hash.match(/^#job\/([a-z0-9_-]+)\/(\d+)$/);
  const view = job ? showJob(job[1], Number(job[2]))
    : location.hash === "#history" ? showHistory() : showServers();
  view.catch(fail);
}

async function signOut() {
  await api("/logout", { method: "POST", body: {} }).catch(() => null);
  showSignIn();
}

async function start() {
  $("sign-out").addEventListener("click", signOut);
  $("confirm-start").addEventListener("click", confirmStart);
  $("job-log-toggle").addEventListener("click", () => toggleLog().catch(fail));
  $("job-cancel").addEventListener("click", () => cancelJob().catch(fail));
  $("add-user").addEventListener("submit", (event) => addUser(event).catch(fail));
  window.addEventListener("hashchange", route);
  try {
    state.me = await api("/me");
  } catch {
    return showSignIn();
  }
  $("who").textContent = `${state.me.email} · ${state.me.role}`;
  route();
}

document.addEventListener("DOMContentLoaded", start);
