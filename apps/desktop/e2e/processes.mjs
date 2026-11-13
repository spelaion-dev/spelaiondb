// What the smoke tests ask of the system about processes, one way per system: asking a window
// to close the way its close button does, and listing processes' command lines.
// On Windows these are the commands smoke.mjs always ran (taskkill without /F, PowerShell's
// Win32_Process); the Linux pass (smoke-linux.mjs) uses the compositor and /proc.

import { execFileSync } from "node:child_process";
import { readdirSync, readFileSync, readlinkSync } from "node:fs";
import { basename } from "node:path";

/** Asks the window of process `pid` to close, as its close button does, so the app's own close
 *  handling runs (it may ask first). Windows: WM_CLOSE through taskkill. Linux: the compositor's
 *  close request (Hyprland, Sway) or the window manager's (X11, wmctrl). */
export function requestClose(pid) {
  if (process.platform === "win32") {
    execFileSync("taskkill", ["/PID", String(pid)], { stdio: "ignore" });
  } else if (process.env.HYPRLAND_INSTANCE_SIGNATURE) {
    const out = execFileSync("hyprctl", ["dispatch", `hl.dsp.window.close({ window = "pid:${pid}" })`], { encoding: "utf8" });
    if (!/^ok\b/.test(out.trim())) throw new Error(`hyprctl could not close the window of ${pid}: ${out.trim()}`);
  } else if (process.env.SWAYSOCK) {
    execFileSync("swaymsg", [`[pid=${pid}] kill`], { stdio: "ignore" });
  } else if (process.env.DISPLAY && !process.env.WAYLAND_DISPLAY) {
    const windows = execFileSync("wmctrl", ["-l", "-p"], { encoding: "utf8" })
      .split("\n")
      .filter((line) => line.split(/\s+/)[2] === String(pid))
      .map((line) => line.split(/\s+/)[0]);
    if (windows.length === 0) throw new Error(`wmctrl lists no window of ${pid}`);
    for (const id of windows) execFileSync("wmctrl", ["-i", "-c", id], { stdio: "ignore" });
  } else {
    throw new Error("no way to ask a window to close here: run under Hyprland, Sway, or X11 with wmctrl");
  }
}

/** The command lines of the running processes whose executable is `name` (without ".exe"). */
export function commandLines(name) {
  if (process.platform === "win32") {
    const out = execFileSync(
      "powershell",
      ["-NoProfile", "-Command", `Get-CimInstance Win32_Process -Filter "Name='${name}.exe'" | ForEach-Object { $_.CommandLine }`],
      { encoding: "utf8" },
    );
    return out.split(/\r?\n/).filter((line) => line.length > 0);
  }
  return linuxProcesses()
    .filter((p) => p.exeName === name || basename(p.argv[0] ?? "") === name)
    .map((p) => p.argv.join(" "));
}

/** Every process this user can see in /proc: pid, parent, executable's name and arguments. */
export function linuxProcesses() {
  const processes = [];
  for (const entry of readdirSync("/proc")) {
    if (!/^\d+$/.test(entry)) continue;
    try {
      const stat = readFileSync(`/proc/${entry}/stat`, "utf8");
      const ppid = Number(stat.slice(stat.lastIndexOf(")") + 2).split(" ")[1]);
      const argv = readFileSync(`/proc/${entry}/cmdline`, "utf8").split("\0").filter((a) => a.length > 0);
      let exe = null;
      try {
        exe = readlinkSync(`/proc/${entry}/exe`);
      } catch {}
      processes.push({ pid: Number(entry), ppid, argv, exe, exeName: exe && basename(exe) });
    } catch {
      // The process ended while it was being read.
    }
  }
  return processes;
}

/** The processes started, directly or not, by process `root`. */
export function descendants(root) {
  const all = linuxProcesses();
  const found = [];
  const parents = new Set([root]);
  for (let grew = true; grew; ) {
    grew = false;
    for (const p of all) {
      if (parents.has(p.ppid) && !parents.has(p.pid)) {
        parents.add(p.pid);
        found.push(p);
        grew = true;
      }
    }
  }
  return found;
}
