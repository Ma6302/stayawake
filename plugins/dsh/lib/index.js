// dsh-stayawake-hint: host-plane plugin for the desktop profile.
//
// stayawake (https://github.com/ local tray app) watches %LOCALAPPDATA%\stayawake\hints\*.hint
// files: a hint younger than hint_ttl_secs (default 60) counts as "an external program is busy".
// opencode writes such a file while it works; DSH did not, so long cloud-LLM turns looked idle.
//
// This plugin keeps dsh-agent.hint fresh while ANY session agent is running (or has queued
// inbox work / background jobs), and lets it expire by TTL once everything goes idle.
// Writes are best-effort and silent: stayawake may be uninstalled at any time.
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const name = "dsh-stayawake-hint";
// No services required; agents/jobs are fetched defensively via ctx.get at use time.
const inject = [];

const REFRESH_MS = 20_000; // < hint_ttl_secs (60s default) with margin for a missed tick
const NOTE = "DeepSeek Harness agent active";

function hintTarget() {
	if (process.platform !== "win32") return undefined;
	if (process.env.DSH_STAYAWAKE_HINT === "0") return undefined;
	const local = process.env.LOCALAPPDATA;
	if (typeof local !== "string" || local.length === 0) return undefined;
	return { dir: join(local, "stayawake", "hints"), file: "dsh-agent.hint" };
}

function refresh(target) {
	try {
		mkdirSync(target.dir, { recursive: true });
		writeFileSync(join(target.dir, target.file), `${NOTE} (${new Date().toISOString()})\n`, { flag: "w" });
	} catch {
		// stayawake gone / permissions changed — nothing to do.
	}
}

function busy(ctx) {
	try {
		const agents = ctx.get("agents");
		const live = agents?.list() ?? [];
		for (const agent of live) {
			if (agent.status === "running") return true;
			if (agent.inbox?.nextTurn?.length > 0) return true;
			if (agent.inbox?.nextStep?.length > 0) return true;
		}
		const jobs = ctx.get("jobs");
		if (jobs !== void 0) {
			for (const agent of [void 0, ...live]) {
				for (const job of jobs.list(agent?.id) ?? []) {
					if (job.status === "running" || job.status === "stopping") return true;
				}
			}
		}
	} catch {
		// services not present (yet) — treat as idle.
	}
	return false;
}

function apply(ctx) {
	const target = hintTarget();
	if (target === undefined) return;
	// Immediate refresh on every status transition, so a just-started turn is
	// noticed without waiting for the interval (and stayawake's own rescan).
	ctx.on("agent/status", () => {
		if (busy(ctx)) refresh(target);
	}, { global: true });
	// New inbox work also counts as busy (queued turn/step) — refresh instantly.
	ctx.on("agent/inbox/inserted", () => {
		if (busy(ctx)) refresh(target);
	}, { global: true });
	// Periodic refresh keeps the file's mtime inside the TTL window during
	// long turns where no status transitions fire.
	const timer = setInterval(() => {
		if (busy(ctx)) refresh(target);
	}, REFRESH_MS);
	timer.unref?.();
	// cordis effect(execute) runs the body immediately and registers the
	// return value as the disposer — RETURN the cleanup, don't run it.
	ctx.effect(() => () => {
		clearInterval(timer);
	});
}

var index = { name, inject, apply };

export { apply, index as default, inject, name };
