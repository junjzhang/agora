pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Layouts
import Quickshell
import Quickshell.Wayland
import Quickshell.Io

WlrLayershell {
    id: root

    namespace: "agora-picker"
    layer: WlrLayer.Overlay
    keyboardFocus: visible ? WlrKeyboardFocus.Exclusive : WlrKeyboardFocus.None
    exclusiveZone: -1
    anchors.top: true
    anchors.bottom: true
    anchors.left: true
    anchors.right: true

    color: visible ? Qt.rgba(0, 0, 0, 0.45) : "transparent"
    Behavior on color { ColorAnimation { duration: 200 } }

    visible: false
    property string mode: "agents"
    property string agentGroupBy: "status"

    function toggle() { visible = !visible }
    function toggleMode(m) {
        if (visible && mode === m) { visible = false; return }
        mode = m
        visible = true
    }

    onVisibleChanged: {
        if (visible) {
            if (_blurRegion) root.BackgroundEffect.blurRegion = _blurRegion
            searchInput.text = ""
            searchInput.forceActiveFocus()
            selectedIndex = 0
            actionMode = false
            refreshData()
        }
    }

    // ── Data ──
    property var projects: []
    property var agents: []
    property var workspaces: []
    property var filteredItems: []
    property int selectedIndex: 0
    property bool actionMode: false
    property int actionIndex: 0
    property var actionList: []
    property string actionRequestKey: ""

    property var selectedItem: filteredItems.length > 0 && selectedIndex < filteredItems.length
        ? filteredItems[selectedIndex] : null

    function refreshData() {
        stateProc.running = true
    }

    Timer {
        interval: 3000
        repeat: true
        running: root.visible
        onTriggered: root.refreshData()
    }

    Process {
        id: stateProc
        command: ["/home/jay/.local/bin/agora", "picker-state"]
        running: false
        stdout: StdioCollector {
            onStreamFinished: {
                try {
                    const d = JSON.parse(text.trim() || "{}")
                    const ps = d.PickerState || d
                    root.projects = ps.projects || []
                    root.agents = ps.agents || []
                    root.workspaces = ps.workspaces || []
                } catch(e) {
                    root.projects = []
                    root.agents = []
                    root.workspaces = []
                }
                root.rebuildItems()
            }
        }
    }
    Process {
        id: actionsProc
        command: []
        running: false
        stdout: StdioCollector {
            onStreamFinished: {
                if (root.actionRequestKey !== root.currentActionTargetKey()) return
                try {
                    const actions = JSON.parse(text.trim() || "[]")
                    root.actionList = root.daemonActionsToRows(actions, root.selectedItem)
                } catch(e) {
                    root.actionList = []
                }
                root.actionIndex = root.firstActionIndex()
            }
        }
    }

    function rebuildItems() {
        const q = searchInput.text.toLowerCase()
        const items = []
        const projectNames = new Set(projects.map(p => p.workspace_name))

        const activeWs = new Set()
        for (const ws of workspaces) { if (ws.name) activeWs.add(ws.name) }

        // Agent map for project badges
        const agentMap = {}
        for (const a of agents) {
            if (!a.project) continue
            const cur = agentMap[a.project]
            if (!cur || phasePrio(a.phase) > phasePrio(cur.phase))
                agentMap[a.project] = a
        }

        // Group agents by project
        const agentsByProject = {}
        const noProjectAgents = []
        for (const a of agents) {
            if (a.project) {
                if (!agentsByProject[a.project]) agentsByProject[a.project] = []
                agentsByProject[a.project].push(a)
            } else {
                noProjectAgents.push(a)
            }
        }

        // ── PROJECTS section ──
        if (root.mode === "projects") {
        const focusedWs = workspaces.find(ws => ws.is_focused)
        const focusedWsName = focusedWs?.name || ""

        function makeProjectEntry(p) {
            const r = p.roots[p.default_root || 0]
            return {
                type: "project",
                name: p.name,
                host: r.host || "",
                path: r.path,
                id: p.id,
                wsName: p.workspace_name,
                agent: agentMap[p.id] || null,
                agents: agentsByProject[p.id] || [],
                active: activeWs.has(p.workspace_name),
                launchers: r.launchers || [],
                ts: p.ts_last_active || 0
            }
        }
        function matchProject(entry) {
            return !q || entry.name.toLowerCase().includes(q) || entry.path.toLowerCase().includes(q)
        }

        const currentProject = focusedWsName ? projects.find(p => p.workspace_name === focusedWsName) : null
        if (currentProject) {
            const entry = makeProjectEntry(currentProject)
            if (matchProject(entry)) {
                items.push({ type: "section", label: "CURRENT" })
                items.push(entry)
            }
        }

        const rest = projects.filter(p => !currentProject || p.id !== currentProject.id)
        if (rest.length > 0) {
            let pushed = false
            for (const p of rest) {
                const entry = makeProjectEntry(p)
                if (!matchProject(entry)) continue
                if (!pushed) { items.push({ type: "section", label: "PROJECTS" }); pushed = true }
                items.push(entry)
            }
        }

        } // end projects mode

        // ── AGENTS section ──
        const allAgents = [...agents]
        const hasAgents = allAgents.length > 0
        if (root.mode === "agents" && hasAgents) {
            function matchQ(a) {
                if (!q) return true
                return a.session_id.toLowerCase().includes(q)
                    || (a.last_prompt || "").toLowerCase().includes(q)
                    || (a.project || "").toLowerCase().includes(q)
            }
            function pushAgent(a) {
                items.push({
                    type: "agent",
                    cli: a.cli || "claude",
                    sessionId: a.session_id,
                    phase: a.phase,
                    project: a.project,
                    host: a.host || "",
                    cwd: a.cwd || "",
                    lastPrompt: a.last_prompt || "",
                    lastMessage: a.last_message || "",
                    slug: a.slug || "",
                    model: a.model || "",
                    startedAt: a.started_at || 0,
                    turnCount: a.turn_count || 0,
                    currentTool: a.current_tool || "",
                    lastChange: a.last_change || 0,
                    effort: a.effort || ""
                })
            }

            if (root.agentGroupBy === "status") {
                const buckets = { "NEEDS YOU": [], "RUNNING": [], "IDLE": [] }
                for (const a of allAgents) {
                    if (!matchQ(a)) continue
                    if (a.phase === "waiting_permission" || a.phase === "waiting_input")
                        buckets["NEEDS YOU"].push(a)
                    else if (a.phase === "running")
                        buckets["RUNNING"].push(a)
                    else
                        buckets["IDLE"].push(a)
                }
                for (const label of ["NEEDS YOU", "RUNNING", "IDLE"]) {
                    if (buckets[label].length === 0) continue
                    items.push({ type: "section", label: label })
                    for (const a of buckets[label]) pushAgent(a)
                }
            } else {
                const agentProjects = Object.keys(agentsByProject).sort()
                for (const projId of agentProjects) {
                    const filtered = agentsByProject[projId].filter(matchQ)
                    if (filtered.length === 0) continue
                    items.push({ type: "section", label: projId })
                    for (const a of filtered) pushAgent(a)
                }
                const filteredNp = noProjectAgents.filter(matchQ)
                if (filteredNp.length > 0) {
                    items.push({ type: "section", label: "(no project)" })
                    for (const a of filteredNp) pushAgent(a)
                }
            }
        }

        // ── WORKSPACES section ──
        const nonProjWs = workspaces.filter(ws => ws.name && !projectNames.has(ws.name))
        if (root.mode === "projects" && nonProjWs.length > 0) {
            items.push({ type: "section", label: "WORKSPACES" })
            for (const ws of nonProjWs) {
                const entry = { type: "workspace", name: ws.name }
                if (!q || entry.name.toLowerCase().includes(q))
                    items.push(entry)
            }
        }

        const prevId = selectedItem?.sessionId || selectedItem?.id || selectedItem?.name || ""
        filteredItems = items
        // Try to keep selection on the same item after refresh.
        let found = -1
        if (prevId) {
            for (let i = 0; i < items.length; i++) {
                const it = items[i]
                if ((it.sessionId || it.id || it.name || "") === prevId) { found = i; break }
            }
        }
        selectedIndex = found >= 0 ? found : firstSelectable()
    }

    function formatDuration(startedAt) {
        root._tick;
        if (!startedAt) return ""
        const secs = Math.floor(Date.now() / 1000) - startedAt
        if (secs < 60) return secs + "s"
        if (secs < 3600) return Math.floor(secs / 60) + "m"
        const h = Math.floor(secs / 3600)
        const m = Math.floor((secs % 3600) / 60)
        return h + "h" + (m > 0 ? m + "m" : "")
    }

    function shortModel(m) {
        if (!m) return ""
        return m.replace("claude-", "").replace(/-\d{8}$/, "")
    }

    function modelColor(m) {
        if (!m) return "#888"
        if (m.includes("opus")) return "#E0A0FF"
        if (m.includes("sonnet")) return "#7EC8E3"
        if (m.includes("haiku")) return "#A8D5A2"
        if (m.includes("gpt-5")) return "#FF8C69"
        if (m.includes("gpt-4")) return "#74AA9C"
        if (m.includes("o3") || m.includes("o4")) return "#FFD700"
        if (m.includes("gpt") || m.includes("codex")) return "#74AA9C"
        return "#ccc"
    }

    property int _tick: 0
    Timer {
        interval: 1000
        repeat: true
        running: root.visible && root.selectedItem?.type === "agent"
        onTriggered: root._tick++
    }

    function timeAgo(ts) {
        root._tick;
        if (!ts) return ""
        const secs = Math.floor(Date.now() / 1000) - ts
        if (secs < 10) return "just now"
        if (secs < 60) return secs + "s ago"
        if (secs < 3600) return Math.floor(secs / 60) + "m ago"
        return Math.floor(secs / 3600) + "h ago"
    }

    function phasePrio(phase) {
        if (phase === "waiting_permission") return 4
        if (phase === "waiting_input") return 3
        if (phase === "running") return 2
        return 1
    }

    function phaseColor(phase) {
        if (phase === "waiting_permission") return "#FFA726"
        if (phase === "waiting_input") return "#EF5350"
        if (phase === "running") return "#66BB6A"
        return "#666"
    }

    function phaseLabel(phase) {
        if (phase === "waiting_permission") return "permission"
        if (phase === "waiting_input") return "waiting"
        if (phase === "running") return "running"
        return "idle"
    }

    function currentActionTargetKey() {
        const item = root.selectedItem
        if (!item || item.type !== "project") return ""
        return "project:" + item.id
    }

    function loadActions(item) {
        if (!item) {
            root.actionList = []
            root.actionIndex = 0
            return
        }
        if (item.type === "project") {
            root.actionRequestKey = "project:" + item.id
            root.actionList = [{ section: "LOADING" }]
            root.actionIndex = 0
            actionsProc.running = false
            actionsProc.command = ["/home/jay/.local/bin/agora", "actions", "project", item.id]
            actionsProc.running = true
            return
        }
        root.actionRequestKey = ""
        root.actionList = root.getActions(item)
        root.actionIndex = root.firstActionIndex()
    }

    function daemonActionsToRows(actions, item) {
        if (!item || item.type !== "project") return []
        const rows = []
        let group = ""
        for (const action of actions) {
            const nextGroup = action.group || "ACTIONS"
            if (nextGroup !== group) {
                rows.push({ section: nextGroup })
                group = nextGroup
            }
            const actionId = action.id || ""
            const targetId = item.id
            rows.push({
                label: action.label || actionId,
                key: action.key || "",
                run: () => root.runDaemonAction("project", targetId, actionId)
            })
        }
        return rows
    }

    function runDaemonAction(target, id, actionId) {
        Quickshell.execDetached(["/home/jay/.local/bin/agora", "run-action", target, id, actionId])
    }

    function getActions(item) {
        if (!item) return []
        if (item.type === "workspace") {
            const name = item.name
            function promote(launchers) {
                let args = "niri msg action focus-workspace '" + name + "' && sleep 0.1 && /home/jay/.local/bin/agora promote --name '" + name + "' --rename-ws"
                for (const l of launchers) args += " --launcher " + l
                args += " ''"
                return () => Quickshell.execDetached(["sh", "-c", args])
            }

            return [
                { section: "NAVIGATE" },
                { label: "Focus workspace", key: "↵", run: () => Quickshell.execDetached(["niri", "msg", "action", "focus-workspace", name]) },
                { section: "PROMOTE" },
                { label: "Promote (bare)", key: "", run: promote([]) },
                { label: "Promote + terminal", key: "", run: promote(["terminal"]) },
                { label: "Promote + VS Code", key: "", run: promote(["vscode", "terminal"]) },
                { label: "Promote + Claude", key: "", run: promote(["claude", "terminal"]) },
                { label: "Promote + Codex", key: "", run: promote(["codex", "terminal"]) },
                { label: "Promote (full stack)", key: "", run: promote(["vscode", "terminal", "claude", "codex"]) },
            ]
        }
        if (item.type === "agent") {
            const actions = [
                { section: "NAVIGATE" },
                { label: "Focus terminal", key: "↵", run: () => Quickshell.execDetached(["/home/jay/.local/bin/agora", "focus-agent", item.sessionId]) },
            ]
            if (item.project) {
                actions.push({ label: "Open project workspace", key: "", run: () => Quickshell.execDetached(["/home/jay/.local/bin/agora", "open", item.project]) })
            }
            actions.push({ section: "INFO" })
            actions.push({ label: "Copy session ID", key: "⌥C", run: () => Quickshell.execDetached(["dms", "cl", "copy", item.sessionId]) })
            if (item.cwd) {
                actions.push({ label: "Copy working directory", key: "", run: () => Quickshell.execDetached(["dms", "cl", "copy", item.cwd]) })
            }
            return actions
        }
        return []
    }

    function isSelectable(item) {
        return item && (item.type === "project" || item.type === "agent" || item.type === "workspace")
    }

    function nextSelectable(from, dir) {
        let i = from + dir
        while (i >= 0 && i < filteredItems.length) {
            if (isSelectable(filteredItems[i])) return i
            i += dir
        }
        return from
    }

    function firstSelectable() {
        for (let i = 0; i < filteredItems.length; i++)
            if (isSelectable(filteredItems[i])) return i
        return 0
    }

    function executeItem(item) {
        if (!item || !isSelectable(item)) return
        if (item.type === "project") {
            root.runDaemonAction("project", item.id, "builtin:open")
            root.visible = false
            return
        }
        if (item.type === "agent") {
            Quickshell.execDetached(["/home/jay/.local/bin/agora", "focus-agent", item.sessionId])
            root.visible = false
            return
        }
        const actions = getActions(item)
        if (actions.length > 0) {
            const first = actions.find(a => a.run)
            if (first) first.run()
        }
        root.visible = false
    }

    function executeAction() {
        if (actionIndex >= 0 && actionIndex < actionList.length && actionList[actionIndex].run) {
            actionList[actionIndex].run()
            root.visible = false
        }
    }

    function nextActionIndex(from, dir) {
        let i = from + dir
        while (i >= 0 && i < actionList.length) {
            if (actionList[i].run) return i
            i += dir
        }
        return from
    }

    function firstActionIndex() {
        for (let i = 0; i < actionList.length; i++)
            if (actionList[i].run) return i
        return 0
    }

    onActionIndexChanged: {
        if (!actionMode || actionIndex < 0) return
        actionListView.positionViewAtIndex(actionIndex, ListView.Contain)
    }

    // ── Scrim ──
    MouseArea {
        anchors.fill: parent
        onClicked: root.visible = false
    }

    property var _blurRegion: null
    function _setupBlur() {
        if (_blurRegion) return
        try {
            const qml = 'import QtQuick; import Quickshell; Region { }'
            const region = Qt.createQmlObject(qml, root, "PickerBlurRegion")
            region.x = Qt.binding(() => panel.x)
            region.y = Qt.binding(() => panel.y)
            region.width = Qt.binding(() => root.visible ? panel.width : 0)
            region.height = Qt.binding(() => root.visible ? panel.height : 0)
            region.radius = 16
            root.BackgroundEffect.blurRegion = region
            _blurRegion = region
        } catch (e) {
            console.warn("Blur not available:", e)
        }
    }
    Component.onCompleted: _setupBlur()

    // ── Main Panel ──
    Rectangle {
        id: panel
        anchors.centerIn: parent
        width: Math.min(parent.width * 0.55, 780)
        height: Math.min(parent.height * 0.58, 540)
        radius: 16
        color: Qt.rgba(0.08, 0.08, 0.08, 0.65)
        border.color: Qt.rgba(1, 1, 1, 0.1)
        border.width: 1
        clip: true

        opacity: root.visible ? 1 : 0
        scale: root.visible ? 1 : 0.95
        Behavior on opacity { NumberAnimation { duration: 200; easing.type: Easing.OutCubic } }
        Behavior on scale { NumberAnimation { duration: 200; easing.type: Easing.OutCubic } }

        Column {
            anchors.fill: parent
            spacing: 0

            // ── Search Bar ──
            Item {
                width: parent.width
                height: 48

                    TextInput {
                        id: searchInput
                        anchors.fill: parent
                        anchors.leftMargin: 20
                        anchors.rightMargin: 20
                        verticalAlignment: TextInput.AlignVCenter
                        color: "#e0e0e0"
                        font.pixelSize: 16
                        clip: true
                        onTextChanged: root.rebuildItems()

                        Keys.onPressed: event => {
                            const shift = event.modifiers & Qt.ShiftModifier
                            if (event.key === Qt.Key_Escape) {
                                if (root.actionMode) { root.actionMode = false; event.accepted = true }
                                else { root.visible = false; event.accepted = true }
                            } else if (event.key === Qt.Key_Tab) {
                                root.actionMode = !root.actionMode
                                if (root.actionMode) {
                                    root.loadActions(root.selectedItem)
                                }
                                event.accepted = true
                            } else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) {
                                if (root.actionMode) root.executeAction()
                                else root.executeItem(root.selectedItem)
                                event.accepted = true
                            } else if (event.key === Qt.Key_Down) {
                                if (root.actionMode) root.actionIndex = root.nextActionIndex(root.actionIndex, 1)
                                else root.selectedIndex = root.nextSelectable(root.selectedIndex, 1)
                                event.accepted = true
                            } else if (event.key === Qt.Key_Up) {
                                if (root.actionMode) root.actionIndex = root.nextActionIndex(root.actionIndex, -1)
                                else root.selectedIndex = root.nextSelectable(root.selectedIndex, -1)
                                event.accepted = true
                            } else if (event.key === Qt.Key_G && (event.modifiers & Qt.ControlModifier)) {
                                if (root.mode === "agents") {
                                    root.agentGroupBy = root.agentGroupBy === "status" ? "project" : "status"
                                    root.rebuildItems()
                                }
                                event.accepted = true
                            }
                        }
                    }

                    Text {
                        anchors.left: parent.left
                        anchors.leftMargin: 20
                        anchors.verticalCenter: parent.verticalCenter
                        text: root.mode === "agents" ? "Search agents..." : "Search projects..."
                        color: "#555"
                        font.pixelSize: 16
                        visible: searchInput.text.length === 0 && !searchInput.preeditText
                    }
            }

            // ── Content: Left list + Right detail ──
            Item {
                width: parent.width
                height: parent.height - 48 - 1 - 40 - 1

                Row {
                    anchors.fill: parent
                    spacing: 0

                    // ── Left: Item List ──
                    Rectangle {
                        id: leftBg
                        width: root.actionMode ? parent.width * 0.35 : parent.width * 0.6
                        Behavior on width { NumberAnimation { duration: 200; easing.type: Easing.OutCubic } }
                        height: parent.height
                        color: root.actionMode ? Qt.rgba(1, 1, 1, 0.02) : Qt.rgba(1, 1, 1, 0.04)
                        Behavior on color { ColorAnimation { duration: 200 } }

                    ListView {
                        id: mainListView
                        anchors.fill: parent
                        anchors.margins: 8
                        clip: true
                        boundsBehavior: Flickable.StopAtBounds
                        model: root.filteredItems
                        spacing: 3
                        currentIndex: root.selectedIndex

                        delegate: Item {
                            id: listItem
                            required property var modelData
                            required property int index
                            width: mainListView.width
                            height: modelData.type === "section" ? 32
                                  : modelData.type === "agent" ? (modelData.lastPrompt ? 52 : 42)
                                  : 42
                            visible: true

                            // ── Section header ──
                            Text {
                                visible: listItem.modelData.type === "section"
                                text: listItem.modelData.label || ""
                                color: {
                                    const l = listItem.modelData.label || ""
                                    if (l === "NEEDS YOU") return "#EF5350"
                                    if (l === "RUNNING") return "#66BB6A"
                                    if (l === "IDLE") return "#888"
                                    return "#777"
                                }
                                font.pixelSize: 12
                                font.weight: Font.Bold
                                font.letterSpacing: 1
                                leftPadding: 8
                                anchors.verticalCenter: parent.verticalCenter
                            }

                            // ── Selectable items (project / agent / workspace) ──
                            Rectangle {
                                visible: listItem.modelData.type === "project" || listItem.modelData.type === "agent" || listItem.modelData.type === "workspace"
                                anchors.fill: parent
                                radius: 8
                                color: root.selectedIndex === listItem.index
                                    ? Qt.rgba(1, 1, 1, 0.14)
                                    : itemMa.containsMouse ? Qt.rgba(1, 1, 1, 0.08) : "transparent"
                                Behavior on color { ColorAnimation { duration: 80 } }

                                MouseArea {
                                    id: itemMa
                                    anchors.fill: parent
                                    hoverEnabled: true
                                    onClicked: root.executeItem(listItem.modelData)
                                    onEntered: { root.selectedIndex = listItem.index; root.actionMode = false }
                                }

                                // Project row
                                Row {
                                    visible: listItem.modelData.type === "project"
                                    anchors.verticalCenter: parent.verticalCenter
                                    anchors.left: parent.left
                                    anchors.right: parent.right
                                    anchors.margins: 10
                                    spacing: 8

                                    Rectangle {
                                        width: 6; height: 6; radius: 3
                                        anchors.verticalCenter: parent.verticalCenter
                                        color: listItem.modelData.agent
                                            ? root.phaseColor(listItem.modelData.agent.phase)
                                            : (listItem.modelData.active ? "#4CAF50" : "#444")
                                    }
                                    Text {
                                        text: listItem.modelData.name || ""
                                        color: root.selectedIndex === listItem.index ? "#fff" : "#ccc"
                                        font.pixelSize: 15
                                        font.weight: Font.Medium
                                        anchors.verticalCenter: parent.verticalCenter
                                        elide: Text.ElideRight
                                    }
                                    Item { width: 1; height: 1 }
                                    Rectangle {
                                        visible: !root.actionMode && (listItem.modelData.agent != null || (listItem.modelData.active || false))
                                        anchors.verticalCenter: parent.verticalCenter
                                        width: projBadge.implicitWidth + 8; height: 16; radius: 8
                                        color: listItem.modelData.agent
                                            ? Qt.rgba(root.phaseColor(listItem.modelData.agent.phase).r || 0.5,
                                                       root.phaseColor(listItem.modelData.agent.phase).g || 0.5,
                                                       root.phaseColor(listItem.modelData.agent.phase).b || 0.5, 0.15)
                                            : Qt.rgba(0.3, 0.69, 0.31, 0.12)
                                        Text {
                                            id: projBadge
                                            anchors.centerIn: parent
                                            text: listItem.modelData.agent ? root.phaseLabel(listItem.modelData.agent.phase) : (listItem.modelData.active ? "active" : "")
                                            font.pixelSize: 10
                                            color: listItem.modelData.agent ? root.phaseColor(listItem.modelData.agent.phase) : "#66BB6A"
                                        }
                                    }
                                }

                                // Agent row
                                Column {
                                    visible: listItem.modelData.type === "agent"
                                    anchors.verticalCenter: parent.verticalCenter
                                    anchors.left: parent.left
                                    anchors.right: parent.right
                                    anchors.margins: 10
                                    spacing: 2

                                    Row {
                                        spacing: 8
                                        Rectangle {
                                            width: 18; height: 18; radius: 4
                                            anchors.verticalCenter: parent.verticalCenter
                                            color: Qt.rgba(root.phaseColor(listItem.modelData.phase || "idle").r || 0.4,
                                                           root.phaseColor(listItem.modelData.phase || "idle").g || 0.4,
                                                           root.phaseColor(listItem.modelData.phase || "idle").b || 0.4, 0.2)
                                            Text {
                                                anchors.centerIn: parent
                                                text: (listItem.modelData.cli || "claude") === "codex" ? "X" : "C"
                                                font.pixelSize: 11
                                                font.weight: Font.Bold
                                                color: root.phaseColor(listItem.modelData.phase || "idle")
                                            }
                                        }
                                        Text {
                                            text: root.agentGroupBy === "status"
                                                ? (listItem.modelData.project || "(no project)")
                                                : root.phaseLabel(listItem.modelData.phase || "idle")
                                            color: root.agentGroupBy === "project"
                                                ? root.phaseColor(listItem.modelData.phase || "idle")
                                                : (root.selectedIndex === listItem.index ? "#fff" : "#ccc")
                                            font.pixelSize: 15
                                            font.weight: Font.Medium
                                            elide: Text.ElideRight
                                        }
                                        Rectangle {
                                            visible: !root.actionMode
                                            anchors.verticalCenter: parent.verticalCenter
                                            width: sidBadge.implicitWidth + 8; height: 16; radius: 8
                                            color: Qt.rgba(1, 1, 1, 0.08)
                                            Text {
                                                id: sidBadge
                                                anchors.centerIn: parent
                                                text: listItem.modelData.slug || ((listItem.modelData.host || "local") + " · " + (listItem.modelData.sessionId || "").substring(0, 8))
                                                font.pixelSize: 10
                                                color: "#888"
                                            }
                                        }
                                        Rectangle {
                                            visible: !root.actionMode && !!(listItem.modelData.model)
                                            anchors.verticalCenter: parent.verticalCenter
                                            width: itemModelLabel.implicitWidth + 8; height: 16; radius: 4
                                            color: Qt.rgba(root.modelColor(listItem.modelData.model || "").r || 0.5,
                                                           root.modelColor(listItem.modelData.model || "").g || 0.5,
                                                           root.modelColor(listItem.modelData.model || "").b || 0.5, 0.12)
                                            Text {
                                                id: itemModelLabel
                                                anchors.centerIn: parent
                                                text: root.shortModel(listItem.modelData.model || "")
                                                font.pixelSize: 10
                                                color: root.modelColor(listItem.modelData.model || "")
                                            }
                                        }
                                        Rectangle {
                                            visible: !root.actionMode && !!(listItem.modelData.effort)
                                            anchors.verticalCenter: parent.verticalCenter
                                            width: itemEffortLabel.implicitWidth + 8; height: 16; radius: 4
                                            color: Qt.rgba(1, 1, 1, 0.06)
                                            Text {
                                                id: itemEffortLabel
                                                anchors.centerIn: parent
                                                text: listItem.modelData.effort || ""
                                                font.pixelSize: 10
                                                color: "#888"
                                            }
                                        }
                                    }
                                    Text {
                                        visible: !!(listItem.modelData.currentTool) || !!(listItem.modelData.lastPrompt)
                                        text: listItem.modelData.currentTool
                                            ? "▸ " + listItem.modelData.currentTool
                                            : (listItem.modelData.lastPrompt || "")
                                        color: listItem.modelData.currentTool ? root.phaseColor("running") : "#888"
                                        font.pixelSize: 13
                                        elide: Text.ElideRight
                                        maximumLineCount: 1
                                        width: parent.width
                                    }
                                }

                                // Workspace row
                                Row {
                                    visible: listItem.modelData.type === "workspace"
                                    anchors.verticalCenter: parent.verticalCenter
                                    anchors.left: parent.left
                                    anchors.right: parent.right
                                    anchors.margins: 10
                                    spacing: 8
                                    Rectangle {
                                        width: 6; height: 6; radius: 3
                                        anchors.verticalCenter: parent.verticalCenter
                                        color: "#444"
                                    }
                                    Text {
                                        text: listItem.modelData.name || ""
                                        color: root.selectedIndex === listItem.index ? "#fff" : "#ccc"
                                        font.pixelSize: 15
                                        font.weight: Font.Medium
                                        anchors.verticalCenter: parent.verticalCenter
                                    }
                                }
                            }
                        }
                    }
                    } // end left bg

                    // ── Vertical Divider ──
                    Rectangle { width: 1; height: parent.height; color: Qt.rgba(1,1,1,0.06) }

                    // ── Right: Detail / Actions ──
                    Rectangle {
                        width: parent.width - leftBg.width - 1
                        height: parent.height
                        color: root.actionMode ? Qt.rgba(1, 1, 1, 0.04) : Qt.rgba(1, 1, 1, 0.01)
                        Behavior on color { ColorAnimation { duration: 200 } }

                    Item {
                        anchors.fill: parent
                        height: parent.height

                        // Detail view (when not in action mode)
                        Column {
                            visible: !root.actionMode && root.selectedItem != null
                            anchors.fill: parent
                            anchors.margins: 20
                            anchors.topMargin: 24
                            spacing: 0

                            // Project detail
                            Column {
                                width: parent.width
                                spacing: 16
                                visible: root.selectedItem?.type === "project"

                                // Path
                                Column {
                                    width: parent.width
                                    spacing: 3
                                    Text { text: "PATH"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                                    Text {
                                        text: {
                                            if (!root.selectedItem || !root.selectedItem.path) return ""
                                            const h = root.selectedItem.host ? root.selectedItem.host + ":" : ""
                                            return h + root.selectedItem.path.replace(/^\/home\/[^\/]+/, "~")
                                        }
                                        color: "#ccc"
                                        font.pixelSize: 14
                                        width: parent.width
                                        elide: Text.ElideMiddle
                                    }
                                }

                                // Agents
                                Column {
                                    width: parent.width
                                    spacing: 6
                                    visible: (root.selectedItem?.agents?.length || 0) > 0

                                    Text { text: "AGENTS  (Shift+↑↓ select, Shift+↵ focus)"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }

                                    Repeater {
                                        model: root.selectedItem?.agents || []
                                        Rectangle {
                                            required property var modelData
                                            required property int index
                                            width: parent.width
                                            height: agentInner.implicitHeight + 6
                                            radius: 6
                                            color: root.agentIndex === index ? Qt.rgba(1, 1, 1, 0.1) : "transparent"
                                            Behavior on color { ColorAnimation { duration: 100 } }

                                            MouseArea {
                                                anchors.fill: parent
                                                hoverEnabled: true
                                                cursorShape: Qt.PointingHandCursor
                                                onEntered: root.agentIndex = index
                                                onClicked: {
                                                    Quickshell.execDetached(["/home/jay/.local/bin/agora", "focus-agent", modelData.session_id])
                                                    root.visible = false
                                                }
                                            }

                                        Column {
                                            id: agentInner
                                            anchors.left: parent.left
                                            anchors.right: parent.right
                                            anchors.verticalCenter: parent.verticalCenter
                                            anchors.margins: 4
                                            spacing: 2
                                            Row {
                                                spacing: 6
                                                Rectangle {
                                                    width: 5; height: 5; radius: 2.5
                                                    anchors.verticalCenter: parent.verticalCenter
                                                    color: root.phaseColor(modelData.phase)
                                                }
                                                Text {
                                                    text: root.phaseLabel(modelData.phase)
                                                    color: root.phaseColor(modelData.phase)
                                                    font.pixelSize: 13
                                                }
                                            }
                                            Text {
                                                text: modelData.last_prompt || ""
                                                color: "#777"
                                                font.pixelSize: 12
                                                elide: Text.ElideRight
                                                maximumLineCount: 1
                                                width: parent.width
                                                visible: !!modelData.last_prompt
                                            }
                                        }
                                        }
                                    }
                                }

                                // Launchers
                                Column {
                                    width: parent.width
                                    spacing: 3
                                    visible: (root.selectedItem?.launchers?.length || 0) > 0
                                    Text { text: "LAUNCHERS"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                                    Row {
                                        spacing: 6
                                        Repeater {
                                            model: root.selectedItem?.launchers || []
                                            Rectangle {
                                                required property var modelData
                                                width: lText.implicitWidth + 10; height: 20; radius: 4
                                                color: Qt.rgba(1,1,1,0.06)
                                                Text {
                                                    id: lText
                                                    anchors.centerIn: parent
                                                    text: modelData
                                                    color: "#aaa"
                                                    font.pixelSize: 12
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // Workspace detail
                            Column {
                                width: parent.width
                                spacing: 8
                                visible: root.selectedItem?.type === "workspace"
                                Text { text: "WORKSPACE"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                                Text {
                                    text: "Not yet tracked as a project."
                                    color: "#888"
                                    font.pixelSize: 14
                                }
                                Text {
                                    text: "Press Tab for promote options"
                                    color: "#666"
                                    font.pixelSize: 13
                                }
                            }

                            // Agent detail
                            Column {
                                width: parent.width
                                spacing: 16
                                visible: root.selectedItem?.type === "agent"

                                Column {
                                    width: parent.width
                                    spacing: 3
                                    Text { text: "SESSION"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                                    Text {
                                        text: root.selectedItem?.sessionId || ""
                                        color: "#ccc"
                                        font.pixelSize: 13
                                        font.family: "monospace"
                                    }
                                }
                                Column {
                                    width: parent.width
                                    spacing: 3
                                    Text { text: "STATUS"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                                    Row {
                                        spacing: 6
                                        Rectangle {
                                            width: 6; height: 6; radius: 3
                                            anchors.verticalCenter: parent.verticalCenter
                                            color: root.phaseColor(root.selectedItem?.phase || "idle")
                                        }
                                        Text {
                                            text: root.phaseLabel(root.selectedItem?.phase || "idle")
                                            color: root.phaseColor(root.selectedItem?.phase || "idle")
                                            font.pixelSize: 14
                                        }
                                    }
                                }
                                Row {
                                    width: parent.width
                                    spacing: 8
                                    visible: !!(root.selectedItem?.model) || !!(root.selectedItem?.effort)
                                    Rectangle {
                                        visible: !!(root.selectedItem?.model)
                                        width: modelLabel.implicitWidth + 12; height: 22; radius: 4
                                        color: Qt.rgba(root.modelColor(root.selectedItem?.model || "").r || 0.5,
                                                       root.modelColor(root.selectedItem?.model || "").g || 0.5,
                                                       root.modelColor(root.selectedItem?.model || "").b || 0.5, 0.15)
                                        anchors.verticalCenter: parent.verticalCenter
                                        Text {
                                            id: modelLabel
                                            anchors.centerIn: parent
                                            text: root.shortModel(root.selectedItem?.model || "")
                                            color: root.modelColor(root.selectedItem?.model || "")
                                            font.pixelSize: 12
                                            font.weight: Font.Medium
                                            font.family: "monospace"
                                        }
                                    }
                                    Rectangle {
                                        visible: !!(root.selectedItem?.effort)
                                        width: effortLabel.implicitWidth + 12; height: 22; radius: 4
                                        color: Qt.rgba(1, 1, 1, 0.06)
                                        anchors.verticalCenter: parent.verticalCenter
                                        Text {
                                            id: effortLabel
                                            anchors.centerIn: parent
                                            text: root.selectedItem?.effort || ""
                                            color: "#aaa"
                                            font.pixelSize: 12
                                            font.weight: Font.Medium
                                        }
                                    }
                                }
                                Row {
                                    width: parent.width
                                    spacing: 16
                                    Column {
                                        spacing: 3
                                        visible: !!(root.selectedItem?.startedAt)
                                        Text { text: "DURATION"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                                        Text { text: root.formatDuration(root.selectedItem?.startedAt || 0); color: "#ccc"; font.pixelSize: 13 }
                                    }
                                    Column {
                                        spacing: 3
                                        visible: (root.selectedItem?.turnCount || 0) > 0
                                        Text { text: "TURNS"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                                        Text { text: String(root.selectedItem?.turnCount || 0); color: "#ccc"; font.pixelSize: 13 }
                                    }
                                    Column {
                                        spacing: 3
                                        visible: !!(root.selectedItem?.lastChange)
                                        Text { text: "LAST ACTIVE"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                                        Text { text: root.timeAgo(root.selectedItem?.lastChange || 0); color: "#888"; font.pixelSize: 13 }
                                    }
                                }
                                Column {
                                    width: parent.width
                                    spacing: 3
                                    visible: !!(root.selectedItem?.currentTool)
                                    Text { text: "CURRENT TOOL"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                                    Text {
                                        text: root.selectedItem?.currentTool || ""
                                        color: root.phaseColor("running")
                                        font.pixelSize: 13
                                        font.family: "monospace"
                                    }
                                }
                                Column {
                                    width: parent.width
                                    spacing: 3
                                    visible: !!(root.selectedItem?.host)
                                    Text { text: "HOST"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                                    Text { text: root.selectedItem?.host || ""; color: "#ccc"; font.pixelSize: 14 }
                                }
                                Column {
                                    width: parent.width
                                    spacing: 3
                                    visible: !!(root.selectedItem?.lastPrompt)
                                    Text { text: "LAST PROMPT"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                                    Text {
                                        text: root.selectedItem?.lastPrompt || ""
                                        color: "#ccc"
                                        font.pixelSize: 13
                                        wrapMode: Text.WordWrap
                                        width: parent.width
                                        maximumLineCount: 3
                                        elide: Text.ElideRight
                                    }
                                }
                            }
                        }

                        // Action list (when in action mode)
                        ListView {
                            id: actionListView
                            visible: root.actionMode
                            anchors.fill: parent
                            anchors.margins: 8
                            clip: true
                            boundsBehavior: Flickable.StopAtBounds
                            model: root.actionList
                            spacing: 2
                            currentIndex: root.actionIndex

                            delegate: Item {
                                id: aItem
                                required property var modelData
                                required property int index
                                width: actionListView.width
                                height: modelData.section ? secText.implicitHeight : 32

                                // Section header
                                Text {
                                    id: secText
                                    visible: !!aItem.modelData.section
                                    text: aItem.modelData.section || ""
                                    color: "#666"
                                    font.pixelSize: 11
                                    font.weight: Font.Bold
                                    font.letterSpacing: 1
                                    leftPadding: 8
                                    topPadding: aItem.index > 0 ? 10 : 4
                                    bottomPadding: 2
                                }

                                // Action row
                                Rectangle {
                                    visible: !aItem.modelData.section
                                    anchors.fill: parent
                                    radius: 8
                                    color: root.actionIndex === aItem.index
                                        ? Qt.rgba(1, 1, 1, 0.14)
                                        : aItemMa.containsMouse ? Qt.rgba(1, 1, 1, 0.08) : "transparent"
                                    Behavior on color { ColorAnimation { duration: 80 } }

                                    MouseArea {
                                        id: aItemMa
                                        anchors.fill: parent
                                        hoverEnabled: true
                                        onClicked: { root.actionIndex = aItem.index; root.executeAction() }
                                        onEntered: root.actionIndex = aItem.index
                                    }

                                    Row {
                                        anchors.verticalCenter: parent.verticalCenter
                                        anchors.left: parent.left
                                        anchors.right: parent.right
                                        anchors.margins: 10
                                        spacing: 8

                                        Text {
                                            text: aItem.modelData.label || ""
                                            color: root.actionIndex === aItem.index ? "#fff" : "#bbb"
                                            font.pixelSize: 14
                                            anchors.verticalCenter: parent.verticalCenter
                                        }

                                        Item { width: 1; height: 1 }

                                        Text {
                                            visible: (aItem.modelData.key || "").length > 0
                                            text: aItem.modelData.key || ""
                                            color: "#555"
                                            font.pixelSize: 12
                                            anchors.verticalCenter: parent.verticalCenter
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                } // end right bg
            }

            // ── Bottom Divider ──
            Rectangle { width: parent.width; height: 1; color: Qt.rgba(1,1,1,0.06) }

            // ── Bottom Action Bar ──
            Rectangle {
                width: parent.width
                height: 40
                color: "transparent"

                Row {
                    anchors.verticalCenter: parent.verticalCenter
                    anchors.right: parent.right
                    anchors.rightMargin: 16
                    spacing: 16
                    layoutDirection: Qt.RightToLeft

                    Row {
                        spacing: 6
                        anchors.verticalCenter: parent.verticalCenter
                        Text { text: root.actionMode ? "Back" : "Actions"; color: "#888"; font.pixelSize: 13; anchors.verticalCenter: parent.verticalCenter }
                        Rectangle {
                            width: tabLabel.implicitWidth + 8; height: 18; radius: 4
                            color: Qt.rgba(1,1,1,0.08)
                            anchors.verticalCenter: parent.verticalCenter
                            Text { id: tabLabel; anchors.centerIn: parent; text: "Tab"; color: "#aaa"; font.pixelSize: 11; font.weight: Font.Medium }
                        }
                    }

                    Row {
                        spacing: 6
                        visible: root.mode === "agents"
                        anchors.verticalCenter: parent.verticalCenter
                        Text { text: "Group: " + root.agentGroupBy; color: "#888"; font.pixelSize: 13; anchors.verticalCenter: parent.verticalCenter }
                        Rectangle {
                            width: grpLabel.implicitWidth + 8; height: 18; radius: 4
                            color: Qt.rgba(1,1,1,0.08)
                            anchors.verticalCenter: parent.verticalCenter
                            Text { id: grpLabel; anchors.centerIn: parent; text: "⌃G"; color: "#aaa"; font.pixelSize: 11; font.weight: Font.Medium }
                        }
                    }

                    Rectangle { width: 1; height: 16; color: Qt.rgba(1,1,1,0.1); anchors.verticalCenter: parent.verticalCenter }

                    Row {
                        spacing: 6
                        anchors.verticalCenter: parent.verticalCenter
                        Text {
                            text: root.selectedItem?.type === "project" ? "Open" : root.selectedItem?.type === "agent" ? "Focus" : "Promote"
                            color: "#888"; font.pixelSize: 13; anchors.verticalCenter: parent.verticalCenter
                        }
                        Rectangle {
                            width: enterLabel.implicitWidth + 8; height: 18; radius: 4
                            color: Qt.rgba(1,1,1,0.08)
                            anchors.verticalCenter: parent.verticalCenter
                            Text { id: enterLabel; anchors.centerIn: parent; text: "↵"; color: "#aaa"; font.pixelSize: 11; font.weight: Font.Medium }
                        }
                    }
                }
            }
        }
    }
}
