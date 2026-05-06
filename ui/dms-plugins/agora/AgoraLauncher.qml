import QtQuick
import Quickshell
import Quickshell.Io
import qs.Services

QtObject {
    id: root

    property var pluginService: null
    property string trigger: "agora"
    property var cachedItems: []
    // Latest agents-by-project map; rebuilt on each agentsProcess run.
    // shape: { project_id: { phase, last_message, count } } where phase is the
    // highest-priority phase among that project's sessions.
    property var agentsByProject: ({})
    // Keep raw projects so we can re-render when agents arrive.
    property var rawProjects: []

    signal itemsChanged

    Component.onCompleted: {
        if (pluginService) {
            trigger = pluginService.loadPluginData("agora", "trigger", "agora")
        }
        refresh()
    }

    function refresh() {
        if (!listProcess.running) listProcess.running = true
        if (!agentsProcess.running) agentsProcess.running = true
        if (!wsProcess.running) wsProcess.running = true
    }

    readonly property var iconMap: ({
        "local":   "material:folder",
        "scratch": "material:edit",
        "code":    "material:code",
        "paper":   "material:description",
        "data":    "material:database",
        "gpu":     "material:memory",
        "build":   "material:dns"
    })

    // Phase priority — higher wins when a project has multiple agents.
    readonly property var phasePriority: ({
        "waiting_permission": 4,
        "waiting_input": 3,
        "running": 2,
        "idle": 1
    })

    readonly property var phaseBadge: ({
        "waiting_permission": "🔒 ",
        "waiting_input": "⚠ ",
        "running": "▶ ",
        "idle": "· "
    })

    // Raw agents array from last poll — needed to build standalone agent items.
    property var rawAgents: []
    // Raw niri workspaces from last poll.
    property var rawWorkspaces: []

    readonly property var phaseLabel: ({
        "waiting_permission": "needs permission",
        "waiting_input": "needs input",
        "running": "running",
        "idle": "idle"
    })

    function buildItems(projects, agentsMap) {
        const items = []

        // 1. Standalone agent items for attention-needed sessions.
        for (const a of rawAgents) {
            if (a.phase !== "waiting_permission" && a.phase !== "waiting_input") continue
            const proj = a.project || "no project"
            const host = a.host || "local"
            const badge = root.phaseBadge[a.phase] || ""
            const label = root.phaseLabel[a.phase] || a.phase
            const sid = a.session_id.substring(0, 8)
            items.push({
                name: badge + proj + "  ·  " + label,
                icon: a.phase === "waiting_permission" ? "material:shield" : "material:priority_high",
                comment: host + " · " + sid + (a.last_message ? "  —  " + a.last_message : ""),
                action: "focus:" + a.session_id,
                categories: ["agora"],
                _priority: root.phasePriority[a.phase] || 0,
                _ts: a.last_change || 0
            })
        }

        // 2. Project items (with agent badge if any).
        for (const p of projects) {
            const r = p.roots[p.default_root || 0]
            const path = r.path
            const host = r.host || ""
            const wsKey = r.label || (host ? "remote" : "local")

            const agent = agentsMap[p.id]
            const badge = agent ? (root.phaseBadge[agent.phase] || "") : ""
            const display = badge + (r.label ? r.label : (host ? host : "local")) + "  ·  " + p.name
            const baseHint = path.replace(/^\/home\/[^\/]+/, "~")
            const hint = agent && agent.last_message
                ? baseHint + "  —  " + agent.last_message
                : (agent ? baseHint + "  —  " + agent.count + " agent(s) " + agent.phase.replace("_", " ") : baseHint)

            const payload = {
                id: p.id,
                ws_name: p.workspace_name,
                path: path,
                host: host
            }
            items.push({
                name: display,
                icon: root.iconMap[wsKey] || "material:cloud",
                comment: hint,
                action: "open:" + JSON.stringify(payload),
                categories: ["agora"],
                _priority: agent ? (root.phasePriority[agent.phase] || 0) : 0,
                _ts: p.ts_last_active || 0
            })
        }

        // 3. Non-project named workspaces.
        const projectWsNames = new Set(projects.map(p => p.workspace_name))
        for (const ws of rawWorkspaces) {
            if (!ws.name || projectWsNames.has(ws.name)) continue
            items.push({
                name: ws.name,
                icon: "material:workspaces",
                comment: "Workspace · not a project · Enter to promote",
                action: "promote:" + ws.name,
                categories: ["agora-ws"],
                _priority: -1,
                _ts: 0
            })
        }

        items.sort((a, b) => {
            if (b._priority !== a._priority) return b._priority - a._priority
            return b._ts - a._ts
        })
        return items
    }

    function rebuild() {
        cachedItems = buildItems(rawProjects, agentsByProject)
        itemsChanged()
    }

    property Process listProcess: Process {
        id: listProcess
        command: ["/home/jay/.local/bin/agora", "list", "--json"]
        running: false
        stdout: StdioCollector {
            onStreamFinished: {
                try {
                    root.rawProjects = JSON.parse(text.trim() || "[]")
                    root.rebuild()
                } catch (e) {
                    console.warn("agora: list parse failed:", e)
                }
            }
        }
    }

    property Process agentsProcess: Process {
        id: agentsProcess
        command: ["/home/jay/.local/bin/agora", "agents", "--json"]
        running: false
        stdout: StdioCollector {
            onStreamFinished: {
                try {
                    const agents = JSON.parse(text.trim() || "[]")
                    root.rawAgents = agents
                    const byProj = {}
                    for (const a of agents) {
                        if (!a.project) continue
                        const cur = byProj[a.project]
                        const prio = root.phasePriority[a.phase] || 0
                        if (!cur || prio > (root.phasePriority[cur.phase] || 0)) {
                            byProj[a.project] = {
                                phase: a.phase,
                                last_message: a.last_message || "",
                                count: 1
                            }
                        }
                        if (cur) cur.count = (cur.count || 1) + 1
                    }
                    root.agentsByProject = byProj
                    root.rebuild()
                } catch (e) {
                    console.warn("agora: agents parse failed:", e)
                }
            }
        }
    }

    property Process wsProcess: Process {
        id: wsProcess
        command: ["niri", "msg", "--json", "workspaces"]
        running: false
        stdout: StdioCollector {
            onStreamFinished: {
                try {
                    root.rawWorkspaces = JSON.parse(text.trim() || "[]")
                    root.rebuild()
                } catch (e) {
                    console.warn("agora: workspaces parse failed:", e)
                }
            }
        }
    }

    // Refresh when the project store changes (daemon writes atomically on every mutation).
    property FileView storeWatcher: FileView {
        path: "/home/jay/.local/share/agora/projects.json"
        watchChanges: true
        onFileChanged: root.refresh()
    }

    function getItems(query) {
        if (!query || query.length === 0) {
            refresh()
            return cachedItems
        }
        const q = query.toLowerCase()
        return cachedItems.filter(item =>
            item.name.toLowerCase().includes(q) ||
            item.comment.toLowerCase().includes(q)
        )
    }

    function parsePayload(action) {
        if (!action) return null
        const idx = action.indexOf(":")
        if (idx < 0) return null
        try {
            return JSON.parse(action.substring(idx + 1))
        } catch (e) {
            return null
        }
    }

    function executeItem(item) {
        const action = item?.action || ""
        if (action.startsWith("focus:")) {
            const sid = action.substring(6)
            Quickshell.execDetached(["/home/jay/.local/bin/agora", "focus-agent", sid])
            return
        }
        if (action.startsWith("promote:")) {
            const wsName = action.substring(8)
            // Focus target workspace first, then promote via shell sequence.
            promoteProcess.command = [
                "sh", "-c",
                "niri msg action focus-workspace '" + wsName + "' && sleep 0.1 && /home/jay/.local/bin/agora promote --name '" + wsName + "' --rename-ws ''"
            ]
            promoteProcess.running = true
            return
        }
        const p = root.parsePayload(action)
        if (!p) return
        Quickshell.execDetached(["/home/jay/.local/bin/agora", "open", p.id])
    }

    function getContextMenuActions(data) {
        const action = data?.action || ""

        // Agent session items
        if (action.startsWith("focus:")) {
            const sid = action.substring(6)
            return [{
                icon: "center_focus_strong",
                text: "Focus agent",
                action: () => Quickshell.execDetached(["/home/jay/.local/bin/agora", "focus-agent", sid]),
                closeLauncher: true
            }]
        }

        // Non-project workspace items
        if (action.startsWith("promote:")) {
            const wsName = action.substring(8)
            function doPromote(launchers) {
                let launcherArgs = ""
                for (const l of launchers) { launcherArgs += " --launcher " + l }
                promoteProcess.command = [
                    "sh", "-c",
                    "niri msg action focus-workspace '" + wsName + "' && sleep 0.1 && /home/jay/.local/bin/agora promote --name '" + wsName + "' --rename-ws" + launcherArgs + " ''"
                ]
                promoteProcess.running = true
            }
            return [
                {
                    icon: "add_circle",
                    text: "Promote (bare)",
                    action: () => doPromote([]),
                    closeLauncher: true
                },
                {
                    icon: "terminal",
                    text: "Promote + terminal",
                    action: () => doPromote(["kitty"]),
                    closeLauncher: true
                },
                {
                    icon: "code",
                    text: "Promote + VS Code",
                    action: () => doPromote(["vscode", "kitty"]),
                    closeLauncher: true
                },
                {
                    icon: "smart_toy",
                    text: "Promote + Claude",
                    action: () => doPromote(["claude", "kitty"]),
                    closeLauncher: true
                },
                {
                    icon: "rocket_launch",
                    text: "Promote (full stack)",
                    action: () => doPromote(["vscode", "kitty", "claude"]),
                    closeLauncher: true
                }
            ]
        }

        // Project items
        const p = root.parsePayload(action)
        if (!p) return []

        const actions = [
            {
                icon: "terminal",
                text: "Open terminal",
                action: () => {
                    if (!p.host) {
                        Quickshell.execDetached(["kitty", "--directory", p.path])
                    } else {
                        Quickshell.execDetached([
                            "kitty", "ssh", p.host,
                            "-t", "cd '" + p.path + "' 2>/dev/null; exec $SHELL"
                        ])
                    }
                },
                closeLauncher: true
            },
            {
                icon: "code",
                text: "Open VS Code",
                action: () => {
                    const uri = p.host
                        ? "vscode-remote://ssh-remote+" + p.host + p.path
                        : "file://" + p.path
                    Quickshell.execDetached(["code", "--folder-uri", uri])
                },
                closeLauncher: true
            },
            {
                icon: "content_copy",
                text: "Copy path",
                action: () => Quickshell.execDetached(["dms", "cl", "copy", p.path]),
                closeLauncher: true
            }
        ]

        if (!p.host) {
            actions.push({
                icon: "folder_open",
                text: "Open in file manager",
                action: () => Quickshell.execDetached(["xdg-open", p.path]),
                closeLauncher: true
            })
        }

        actions.push({
            icon: "delete_outline",
            text: "Forget project",
            action: () => {
                forgetProcess.command = ["/home/jay/.local/bin/agora", "forget", p.id]
                forgetProcess.running = true
            },
            closeLauncher: false
        })

        return actions
    }

    property Process promoteProcess: Process {
        id: promoteProcess
        running: false
        onExited: exitCode => {
            if (exitCode === 0) {
                root.refresh()
            } else {
                console.warn("agora: promote failed with exit code", exitCode)
            }
        }
    }

    property Process forgetProcess: Process {
        id: forgetProcess
        running: false
        onExited: exitCode => {
            if (exitCode === 0) {
                root.refresh()
            } else {
                console.warn("agora: forget failed with exit code", exitCode)
            }
        }
    }

    onTriggerChanged: {
        if (pluginService) {
            pluginService.savePluginData("agora", "trigger", trigger)
        }
    }
}
