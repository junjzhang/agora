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
    Behavior on color { ColorAnimation { duration: root.animScrim } }

    visible: false
    property string mode: "agents"
    property string agentGroupBy: "status"

    // Tool paths. Picker QML lives outside the cargo build, so we don't get
    // these from env. If the user installs elsewhere, override here.
    readonly property string agora: "/home/jay/.local/bin/agora"
    readonly property string dms: "dms"

    // Animation durations. One source of truth — change feel here.
    readonly property int animScrim: 200       // scrim color fade
    readonly property int animPanelOpen: 140   // main panel opacity + scale entrance
    readonly property int animColResize: 150   // column width / toast height
    readonly property int animPanelFadeIn: 110 // sub-panel opacity from 0→1 on creation
    readonly property int animToastFade: 220   // toast opacity
    readonly property int animTint: 100        // input focus tint, breadcrumb, launchers
    readonly property int animHover: 80        // list-item hover

    function toggle() { visible = !visible }
    function toggleMode(m) {
        if (visible && mode === m) { visible = false; return }
        mode = m
        visible = true
    }

    onVisibleChanged: {
        if (visible) {
            if (_blurRegion) root.BackgroundEffect.blurRegion = _blurRegion
            searchText = ""
            selectedIndex = 0
            panelStack = [{ kind: "main", title: root.mode === "agents" ? "Agents" : "Projects" }]
            refreshData()
            Qt.callLater(() => { if (searchInput) searchInput.forceActiveFocus() })
        } else {
            // Collapse to depth 1 so prevSlot/topSlot Loaders deactivate and
            // the QML tree is small while the picker is idle. Saves work for
            // the compositor when other overlays (dms spotlight, etc.) animate.
            panelStack = [{ kind: "main", title: "" }]
            clearToast()
        }
    }

    // ── Panel stack ──
    // The picker is a navigation stack. Tab pushes a new panel (drill in),
    // Esc/Shift+Tab pops back. Slot Loaders dispatch on topPanel.kind, and
    // the breadcrumb in the footer shows the path through the stack.
    //
    // Panel shape:
    //   { kind: "main" | "actions" | "promote",
    //     title: string,           // breadcrumb chip label
    //     data: { ... } }          // panel-specific payload
    property var panelStack: [{ kind: "main", title: "" }]
    readonly property var topPanel: panelStack[panelStack.length - 1]

    function pushPanel(p) {
        panelStack = panelStack.concat([p])
    }
    function popPanel() {
        if (panelStack.length <= 1) { root.visible = false; return }
        panelStack = panelStack.slice(0, -1)
        // Return focus to the always-visible search bar (a deeper panel may
        // have stolen it, e.g. PanelPromote's name input).
        Qt.callLater(() => { if (searchInput) searchInput.forceActiveFocus() })
    }
    function popToDepth(depth) {
        if (depth < 1) depth = 1
        if (depth >= panelStack.length) return
        panelStack = panelStack.slice(0, depth)
    }

    readonly property bool actionMode: topPanel.kind === "actions"

    // ── Data ──
    property var projects: []
    property var agents: []
    property var workspaces: []
    property var filteredItems: []
    property int selectedIndex: 0
    property int actionIndex: 0
    property var actionList: []
    property string actionRequestKey: ""
    // Search box text lives at root so it survives PanelMain's Loader being
    // destroyed/recreated when the panel goes from full → compact and back.
    property string searchText: ""
    onSearchTextChanged: {
        rebuildItems()
        if (toastKind === "error") clearToast()
    }

    // ── Toast: cross-panel error/info banner ──
    // info  → auto-dismiss after 4s.
    // error → stays until next user action / manual dismiss / picker close.
    property string toastText: ""
    property string toastKind: "info"
    property bool   toastShown: false

    function reportInfo(msg) {
        toastText = msg
        toastKind = "info"
        toastShown = true
        toastAutoClear.restart()
    }
    function reportError(msg) {
        toastText = msg
        toastKind = "error"
        toastShown = true
        toastAutoClear.stop()
    }
    function clearToast() { toastShown = false }

    Timer {
        id: toastAutoClear
        interval: 4000
        onTriggered: root.clearToast()
    }
    // Switching panels also clears a sticky error — the user moved on.
    onPanelStackChanged: if (toastKind === "error") clearToast()

    property var selectedItem: filteredItems.length > 0 && selectedIndex < filteredItems.length
        ? filteredItems[selectedIndex] : null

    // Entry point used by PanelMain's workspace action.
    function openPromoteWizard(item) {
        pushPanel({
            kind: "promote",
            title: "Promote",
            data: {
                wsId: item.wsId || 0,
                // What we pass to `niri msg action focus-workspace`. niri
                // takes either a name (only set when the ws was explicitly
                // named) or an idx string — so prefer name, fall back to idx.
                wsRef: item.name || String(item.idx || ""),
                wsName: item.name || "",
                seedName: item.name || "",
            },
        })
    }

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
        command: [root.agora, "picker-state"]
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
    // Stable identity for an item, used to keep the selection across
    // rebuilds. Section rows have no key (they're not selectable anyway).
    function itemKey(it) {
        return it?.sessionId || it?.id || it?.name || ""
    }

    // Breadcrumb title for an item — used when pushing into an actions panel.
    function panelTitleFor(it) {
        if (!it) return ""
        if (it.type === "project") return it.name || it.id || "Project"
        if (it.type === "agent") return (it.slug || it.sessionId || "Agent").slice(0, 24)
        if (it.type === "workspace") return it.label || it.name || ("Workspace " + (it.idx || ""))
        return ""
    }

    function rebuildItems() {
        const q = (root.searchText || "").toLowerCase()
        const items = []
        const projectNames = new Set(projects.map(p => p.workspace_name))

        // Agent map for project badges + agents grouped by project for the
        // AGENTS section.
        const agentMap = {}
        const agentsByProject = {}
        const noProjectAgents = []
        for (const a of agents) {
            if (a.project) {
                const cur = agentMap[a.project]
                if (!cur || phasePrio(a.phase) > phasePrio(cur.phase))
                    agentMap[a.project] = a
                if (!agentsByProject[a.project]) agentsByProject[a.project] = []
                agentsByProject[a.project].push(a)
            } else {
                noProjectAgents.push(a)
            }
        }

        const focusedWs = workspaces.find(ws => ws.is_focused)
        const focusedWsName = focusedWs?.name || ""
        const currentProject = focusedWsName ? projects.find(p => p.workspace_name === focusedWsName) : null

        // Section helper: push the header lazily on the first matched entry
        // so we don't end up with empty sections.
        function pushGroup(label, rows) {
            let pushed = false
            for (const r of rows) {
                if (!r) continue
                if (!pushed) { items.push({ type: "section", label: label }); pushed = true }
                items.push(r)
            }
        }

        function projectEntry(p) {
            const r = p.roots[p.default_root || 0]
            const entry = {
                type: "project",
                name: p.name,
                host: r.host || "",
                path: r.path,
                id: p.id,
                wsName: p.workspace_name,
                agent: agentMap[p.id] || null,
                agents: agentsByProject[p.id] || [],
                active: workspaces.some(ws => ws.name === p.workspace_name),
                launchers: r.launchers || [],
                ts: p.ts_last_active || 0
            }
            const matched = !q || entry.name.toLowerCase().includes(q) || entry.path.toLowerCase().includes(q)
            return matched ? entry : null
        }
        function wsEntry(ws) {
            const label = ws.name || ("Workspace " + ws.idx)
            const entry = { type: "workspace", name: ws.name || "", label: label, wsId: ws.id, idx: ws.idx }
            const matched = !q || label.toLowerCase().includes(q)
            return matched ? entry : null
        }
        function agentEntry(a) {
            const matched = !q
                || a.session_id.toLowerCase().includes(q)
                || (a.last_prompt || "").toLowerCase().includes(q)
                || (a.project || "").toLowerCase().includes(q)
            if (!matched) return null
            return {
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
            }
        }

        if (root.mode === "projects") {
            // CURRENT: focused workspace, either as project or as bare ws.
            const currentRow = currentProject ? projectEntry(currentProject)
                : focusedWs ? wsEntry(focusedWs)
                : null
            pushGroup("CURRENT", [currentRow])

            // PROJECTS: everything except the current project.
            pushGroup("PROJECTS",
                projects
                    .filter(p => !currentProject || p.id !== currentProject.id)
                    .map(projectEntry))

            // WORKSPACES: non-project workspaces with at least one window.
            // The focused (non-project) ws already lives in CURRENT.
            pushGroup("WORKSPACES",
                workspaces
                    .filter(ws => {
                        if (ws.is_focused && !currentProject) return false
                        if (ws.name && projectNames.has(ws.name)) return false
                        return (ws.window_count || 0) > 0
                    })
                    .map(wsEntry))
        }

        if (root.mode === "agents" && agents.length > 0) {
            if (root.agentGroupBy === "status") {
                const buckets = { "NEEDS YOU": [], "RUNNING": [], "IDLE": [] }
                for (const a of agents) {
                    const row = agentEntry(a)
                    if (!row) continue
                    if (a.phase === "waiting_permission" || a.phase === "waiting_input")
                        buckets["NEEDS YOU"].push(row)
                    else if (a.phase === "running")
                        buckets["RUNNING"].push(row)
                    else
                        buckets["IDLE"].push(row)
                }
                for (const label of ["NEEDS YOU", "RUNNING", "IDLE"]) pushGroup(label, buckets[label])
            } else {
                for (const projId of Object.keys(agentsByProject).sort()) {
                    pushGroup(projId, agentsByProject[projId].map(agentEntry))
                }
                pushGroup("(no project)", noProjectAgents.map(agentEntry))
            }
        }

        filteredItems = items
        // Keep selection on the same item across rebuilds when possible.
        const prevId = itemKey(selectedItem)
        let found = -1
        if (prevId) {
            for (let i = 0; i < items.length; i++) {
                if (itemKey(items[i]) === prevId) { found = i; break }
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

    /// Translucent variant of `phaseColor(phase)`. Avoids the 3×phaseColor
    /// pattern that was sprinkled through the delegate code.
    function phaseTint(phase, alpha) {
        const c = phaseColor(phase)
        return Qt.rgba(c.r || 0.5, c.g || 0.5, c.b || 0.5, alpha)
    }

    function modelTint(model, alpha) {
        const c = modelColor(model)
        return Qt.rgba(c.r || 0.5, c.g || 0.5, c.b || 0.5, alpha)
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
            actionsProc.command = [root.agora, "actions", "project", item.id]
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
        Quickshell.execDetached([root.agora, "run-action", target, id, actionId])
    }

    function getActions(item) {
        if (!item) return []
        if (item.type === "workspace") {
            const name = item.name || ""
            const focusRef = name || String(item.idx)
            return [
                { section: "NAVIGATE" },
                { label: "Focus workspace", key: "↵", run: () => Quickshell.execDetached(["niri", "msg", "action", "focus-workspace", focusRef]) },
                { section: "PROMOTE" },
                { label: "Promote workspace...", key: "Tab", tab: true, run: () => root.openPromoteWizard(item), keepOpen: true },
            ]
        }
        if (item.type === "agent") {
            const actions = [
                { section: "NAVIGATE" },
                { label: "Focus terminal", key: "↵", run: () => Quickshell.execDetached([root.agora, "focus-agent", item.sessionId]) },
            ]
            if (item.project) {
                actions.push({ label: "Open project workspace", key: "", run: () => Quickshell.execDetached([root.agora, "open", item.project]) })
            }
            actions.push({ section: "INFO" })
            actions.push({ label: "Copy session ID", key: "⌥C", run: () => Quickshell.execDetached([root.dms, "cl", "copy", item.sessionId]) })
            if (item.cwd) {
                actions.push({ label: "Copy working directory", key: "", run: () => Quickshell.execDetached([root.dms, "cl", "copy", item.cwd]) })
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
            Quickshell.execDetached([root.agora, "focus-agent", item.sessionId])
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
            const act = actionList[actionIndex]
            act.run()
            if (!act.keepOpen) root.visible = false
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

    // Scroll-into-view is now handled inside PanelActions itself (currentIndex
    // binding takes care of it for ListView).

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
        width: Math.min(parent.width * 0.6, 880)
        height: Math.min(parent.height * 0.6, 580)
        radius: 16
        color: Qt.rgba(0.08, 0.08, 0.08, 0.65)
        border.color: Qt.rgba(1, 1, 1, 0.1)
        border.width: 1
        clip: true

        opacity: root.visible ? 1 : 0
        scale: root.visible ? 1 : 0.95
        Behavior on opacity { NumberAnimation { duration: root.animPanelOpen; easing.type: Easing.OutCubic } }
        Behavior on scale { NumberAnimation { duration: root.animPanelOpen; easing.type: Easing.OutCubic } }

        Column {
            anchors.fill: parent
            spacing: 0

            // ── Search Bar (fixed top, spans full width regardless of stack depth) ──
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
                    text: root.searchText
                    onTextChanged: root.searchText = text

                    Keys.onPressed: event => {
                        if (event.key === Qt.Key_Escape) {
                            root.popPanel()
                            event.accepted = true
                        } else if (event.key === Qt.Key_Tab) {
                            const top = root.topPanel.kind
                            if (top === "main") {
                                if (root.selectedItem) {
                                    root.pushPanel({
                                        kind: "actions",
                                        title: root.panelTitleFor(root.selectedItem),
                                    })
                                    root.loadActions(root.selectedItem)
                                }
                            } else if (top === "actions") {
                                const cur = root.actionList[root.actionIndex]
                                if (cur && cur.tab && cur.run) {
                                    cur.run()
                                    if (!cur.keepOpen) root.visible = false
                                } else {
                                    root.popPanel()
                                }
                            }
                            event.accepted = true
                        } else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) {
                            const top = root.topPanel.kind
                            if (top === "main") root.executeItem(root.selectedItem)
                            else if (top === "actions") root.executeAction()
                            event.accepted = true
                        } else if (event.key === Qt.Key_Down) {
                            if (root.topPanel.kind === "actions") {
                                root.actionIndex = root.nextActionIndex(root.actionIndex, 1)
                            } else {
                                root.selectedIndex = root.nextSelectable(root.selectedIndex, 1)
                            }
                            event.accepted = true
                        } else if (event.key === Qt.Key_Up) {
                            if (root.topPanel.kind === "actions") {
                                root.actionIndex = root.nextActionIndex(root.actionIndex, -1)
                            } else {
                                root.selectedIndex = root.nextSelectable(root.selectedIndex, -1)
                            }
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

            Rectangle { width: parent.width; height: 1; color: Qt.rgba(1, 1, 1, 0.06) }

            // ── Body: panel slots ──
            // Two-column stack view. Left = panelStack[depth-2] in compact
            // mode (visible only when depth >= 2); right = top panel in full
            // mode. Beyond depth 2 the deeper ancestors are not rendered;
            // breadcrumb in the footer remains the source of truth.
            Item {
                width: parent.width
                height: parent.height - 48 - 1 - toastBar.height - 40 - 1

                Row {
                    anchors.fill: parent
                    spacing: 0

                    // Main panel lives outside the Loader system: its
                    // ListView delegate is heavy and rebuilding it on every
                    // Tab is what made the previous design feel laggy.
                    // Instead it stays alive and we toggle compact + width.
                    PanelMain {
                        id: mainPanel
                        picker: root
                        readonly property int prevIdx: root.panelStack.length - 2
                        readonly property bool isTop: root.topPanel.kind === "main"
                        readonly property bool isPrev: prevIdx >= 0 && root.panelStack[prevIdx].kind === "main"
                        compact: !isTop && isPrev
                        visible: isTop || isPrev
                        width: isTop ? parent.width : (isPrev ? parent.width * 0.35 : 0)
                        Behavior on width { NumberAnimation { duration: root.animColResize; easing.type: Easing.OutCubic } }
                        height: parent.height
                    }

                    Rectangle {
                        visible: mainPanel.visible && (prevSlot.visible || topSlot.visible)
                        width: 1
                        height: parent.height
                        color: Qt.rgba(1, 1, 1, 0.06)
                    }

                    // Previous-layer slot — only used when prev is *not* main
                    // (e.g. depth=3 with prev=actions). When prev==main, the
                    // sibling PanelMain already handles it.
                    Loader {
                        id: prevSlot
                        readonly property int prevIdx: root.panelStack.length - 2
                        readonly property string prevKind: prevIdx >= 0 ? root.panelStack[prevIdx].kind : ""
                        visible: root.panelStack.length >= 2 && prevKind !== "main"
                        active: visible
                        width: visible ? parent.width * 0.35 : 0
                        Behavior on width { NumberAnimation { duration: root.animColResize; easing.type: Easing.OutCubic } }
                        height: parent.height
                        sourceComponent: {
                            switch (prevKind) {
                                case "actions": return actionsComp
                                case "promote": return promoteComp
                            }
                            return null
                        }
                        onLoaded: if (item && item.hasOwnProperty("compact")) item.compact = true
                    }

                    Rectangle {
                        visible: prevSlot.visible && topSlot.visible
                        width: 1
                        height: parent.height
                        color: Qt.rgba(1, 1, 1, 0.06)
                    }

                    // Top-layer slot — for everything but main on top.
                    Loader {
                        id: topSlot
                        readonly property string topKind: root.topPanel.kind
                        visible: topKind !== "main"
                        active: visible
                        readonly property int dividerCount:
                            (mainPanel.visible ? 1 : 0) + (prevSlot.visible ? 1 : 0)
                        width: visible
                            ? parent.width - mainPanel.width - prevSlot.width - dividerCount
                            : 0
                        Behavior on width { NumberAnimation { duration: root.animColResize; easing.type: Easing.OutCubic } }
                        height: parent.height
                        sourceComponent: {
                            switch (topKind) {
                                case "actions": return actionsComp
                                case "promote": return promoteComp
                            }
                            return null
                        }
                        onLoaded: if (item && item.hasOwnProperty("compact")) item.compact = false
                    }
                }
            }


            // ── Toast banner ──
            // Cross-panel error/info display. Slides in above the footer.
            // info auto-dismisses; error stays until next user action.
            Rectangle {
                id: toastBar
                width: parent.width
                height: root.toastShown ? 32 : 0
                Behavior on height { NumberAnimation { duration: root.animColResize; easing.type: Easing.OutCubic } }
                opacity: root.toastShown ? 1 : 0
                Behavior on opacity { NumberAnimation { duration: root.animToastFade; easing.type: Easing.OutCubic } }
                clip: true
                color: root.toastKind === "error"
                    ? Qt.rgba(0.95, 0.3, 0.3, 0.18)
                    : Qt.rgba(0.4, 0.7, 1, 0.15)

                Row {
                    anchors.fill: parent
                    anchors.leftMargin: 14
                    anchors.rightMargin: 8
                    spacing: 8
                    Text {
                        text: root.toastKind === "error" ? "⚠" : "ℹ"
                        color: root.toastKind === "error" ? "#FFB4B4" : "#9ECEFF"
                        font.pixelSize: 13
                        anchors.verticalCenter: parent.verticalCenter
                    }
                    Text {
                        width: toastBar.width - 14 - 8 - 14 - 24
                        anchors.verticalCenter: parent.verticalCenter
                        text: root.toastText
                        color: root.toastKind === "error" ? "#FFB4B4" : "#9ECEFF"
                        font.pixelSize: 12
                        elide: Text.ElideRight
                    }
                }
                Rectangle {
                    width: 20; height: 20; radius: 4
                    anchors.right: parent.right
                    anchors.rightMargin: 8
                    anchors.verticalCenter: parent.verticalCenter
                    color: dismissMa.containsMouse ? Qt.rgba(1,1,1,0.1) : "transparent"
                    visible: root.toastShown
                    Text {
                        anchors.centerIn: parent
                        text: "×"
                        color: "#aaa"
                        font.pixelSize: 14
                    }
                    MouseArea {
                        id: dismissMa
                        anchors.fill: parent
                        hoverEnabled: true
                        cursorShape: Qt.PointingHandCursor
                        onClicked: root.clearToast()
                    }
                }
            }

            // ── Bottom Divider ──
            Rectangle { width: parent.width; height: 1; color: Qt.rgba(1,1,1,0.06) }

            // ── Bottom Action Bar ──
            // Left side: breadcrumb (panel stack path). Right side: keyboard
            // hints. Both share one fixed-height row so the bar never moves.
            Rectangle {
                width: parent.width
                height: 40
                color: "transparent"

                // Breadcrumb (left).
                Row {
                    anchors.left: parent.left
                    anchors.verticalCenter: parent.verticalCenter
                    anchors.leftMargin: 16
                    spacing: 4
                    Repeater {
                        model: root.panelStack
                        Row {
                            required property var modelData
                            required property int index
                            spacing: 4
                            anchors.verticalCenter: parent.verticalCenter
                            Text {
                                visible: parent.index > 0
                                text: "›"
                                color: "#444"
                                font.pixelSize: 13
                                anchors.verticalCenter: parent.verticalCenter
                            }
                            Rectangle {
                                readonly property bool isTop: parent.index === root.panelStack.length - 1
                                radius: 5
                                height: 20
                                width: bcText.implicitWidth + 14
                                anchors.verticalCenter: parent.verticalCenter
                                color: isTop
                                    ? Qt.rgba(0.4, 0.7, 1, 0.22)
                                    : bcMa.containsMouse ? Qt.rgba(1,1,1,0.08) : Qt.rgba(1,1,1,0.03)
                                Behavior on color { ColorAnimation { duration: root.animTint } }
                                Text {
                                    id: bcText
                                    anchors.centerIn: parent
                                    text: parent.parent.modelData.title || ""
                                    color: parent.isTop ? "#fff" : "#999"
                                    font.pixelSize: 11
                                    font.weight: parent.isTop ? Font.Medium : Font.Normal
                                }
                                MouseArea {
                                    id: bcMa
                                    anchors.fill: parent
                                    hoverEnabled: true
                                    cursorShape: parent.isTop ? Qt.ArrowCursor : Qt.PointingHandCursor
                                    onClicked: {
                                        if (!parent.isTop) root.popToDepth(parent.parent.index + 1)
                                    }
                                }
                            }
                        }
                    }
                }

                // Keyboard hints (right).
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

    // ── Panel components ──
    // Each panel kind has its inline component below. The Component { id:
    // ... } wrappers expose them to Loader.sourceComponent. Panels receive
    // a `picker` ref to call into root state (push/pop, projects, …) and
    // their per-panel input via `data: ...`.

    /// Root list panel — renders the projects/agents/workspaces list plus the
    /// selected-item detail. Search bar is owned by the root frame (always
    /// visible), so this panel is purely about content. `compact` collapses
    /// the detail column to 0 width when this panel is showing as the
    /// previous layer of the stack.
    component PanelMain: Item {
        id: panel
        required property var picker
        property bool compact: false

        Item {
            anchors.fill: parent

            Row {
                anchors.fill: parent
                spacing: 0

                    Rectangle {
                        id: listBg
                        width: panel.compact ? parent.width : parent.width * 0.6
                        Behavior on width { NumberAnimation { duration: root.animColResize; easing.type: Easing.OutCubic } }
                        height: parent.height
                        color: Qt.rgba(1, 1, 1, 0.04)

                        ListView {
                            id: mainListView
                            anchors.fill: parent
                            anchors.margins: 8
                            clip: true
                            boundsBehavior: Flickable.StopAtBounds
                            model: panel.picker.filteredItems
                            spacing: 3
                            currentIndex: panel.picker.selectedIndex
                            onCurrentIndexChanged: positionViewAtIndex(currentIndex, ListView.Contain)

                            delegate: Item {
                                id: listItem
                                required property var modelData
                                required property int index
                                width: mainListView.width
                                height: modelData.type === "section" ? 32
                                      : modelData.type === "agent" ? ((modelData.currentTool || modelData.lastPrompt) ? 52 : 42)
                                      : 42
                                visible: true

                                // Cache the derived phase/model lookups so we
                                // don't call the helpers 3+ times per delegate.
                                readonly property string _phase: modelData.phase || "idle"
                                readonly property color  _phaseClr: panel.picker.phaseColor(_phase)
                                readonly property string _agentPhase: modelData.agent ? modelData.agent.phase : ""
                                readonly property color  _agentPhaseClr: _agentPhase
                                    ? panel.picker.phaseColor(_agentPhase)
                                    : (modelData.active ? "#4CAF50" : "#444")
                                readonly property bool   _selected: panel.picker.selectedIndex === index

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

                                Rectangle {
                                    visible: listItem.modelData.type === "project" || listItem.modelData.type === "agent" || listItem.modelData.type === "workspace"
                                    anchors.fill: parent
                                    radius: 8
                                    color: listItem._selected
                                        ? Qt.rgba(1, 1, 1, 0.14)
                                        : itemMa.containsMouse ? Qt.rgba(1, 1, 1, 0.08) : "transparent"
                                    Behavior on color { ColorAnimation { duration: root.animHover } }

                                    MouseArea {
                                        id: itemMa
                                        anchors.fill: parent
                                        hoverEnabled: true
                                        onClicked: panel.picker.executeItem(listItem.modelData)
                                        onEntered: panel.picker.selectedIndex = listItem.index
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
                                            color: listItem._agentPhaseClr
                                        }
                                        Text {
                                            text: listItem.modelData.name || ""
                                            color: listItem._selected ? "#fff" : "#ccc"
                                            font.pixelSize: 15
                                            font.weight: Font.Medium
                                            anchors.verticalCenter: parent.verticalCenter
                                            elide: Text.ElideRight
                                        }
                                        Item { width: 1; height: 1 }
                                        Rectangle {
                                            visible: !panel.compact && (listItem._agentPhase !== "" || (listItem.modelData.active || false))
                                            anchors.verticalCenter: parent.verticalCenter
                                            width: projBadge.implicitWidth + 8; height: 16; radius: 8
                                            color: listItem._agentPhase
                                                ? panel.picker.phaseTint(listItem._agentPhase, 0.15)
                                                : Qt.rgba(0.3, 0.69, 0.31, 0.12)
                                            Text {
                                                id: projBadge
                                                anchors.centerIn: parent
                                                text: listItem._agentPhase
                                                    ? panel.picker.phaseLabel(listItem._agentPhase)
                                                    : (listItem.modelData.active ? "active" : "")
                                                font.pixelSize: 10
                                                color: listItem._agentPhase ? listItem._agentPhaseClr : "#66BB6A"
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
                                                color: panel.picker.phaseTint(listItem._phase, 0.2)
                                                Text {
                                                    anchors.centerIn: parent
                                                    text: (listItem.modelData.cli || "claude") === "codex" ? "X" : "C"
                                                    font.pixelSize: 11
                                                    font.weight: Font.Bold
                                                    color: listItem._phaseClr
                                                }
                                            }
                                            Text {
                                                text: panel.picker.agentGroupBy === "status"
                                                    ? (listItem.modelData.project || "(no project)")
                                                    : panel.picker.phaseLabel(listItem._phase)
                                                color: panel.picker.agentGroupBy === "project"
                                                    ? listItem._phaseClr
                                                    : (listItem._selected ? "#fff" : "#ccc")
                                                font.pixelSize: 15
                                                font.weight: Font.Medium
                                                elide: Text.ElideRight
                                            }
                                            Rectangle {
                                                visible: !panel.compact
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
                                                readonly property color _modelClr: panel.picker.modelColor(listItem.modelData.model || "")
                                                visible: !panel.compact && !!(listItem.modelData.model)
                                                anchors.verticalCenter: parent.verticalCenter
                                                width: itemModelLabel.implicitWidth + 8; height: 16; radius: 4
                                                color: panel.picker.modelTint(listItem.modelData.model || "", 0.12)
                                                Text {
                                                    id: itemModelLabel
                                                    anchors.centerIn: parent
                                                    text: panel.picker.shortModel(listItem.modelData.model || "")
                                                    font.pixelSize: 10
                                                    color: parent._modelClr
                                                }
                                            }
                                            Rectangle {
                                                visible: !panel.compact && !!(listItem.modelData.effort)
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
                                            color: listItem.modelData.currentTool ? "#66BB6A" : "#888"
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
                                            text: listItem.modelData.label || listItem.modelData.name || ""
                                            color: listItem._selected ? "#fff" : "#ccc"
                                            font.pixelSize: 15
                                            font.weight: Font.Medium
                                            anchors.verticalCenter: parent.verticalCenter
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // ── Detail panel (collapses to 0 width when compact) ──
                    Rectangle {
                        width: panel.compact ? 0 : 1
                        Behavior on width { NumberAnimation { duration: root.animColResize; easing.type: Easing.OutCubic } }
                        height: parent.height
                        color: Qt.rgba(1,1,1,0.06)
                        clip: true
                    }
                    Rectangle {
                        id: detailBg
                        width: panel.compact ? 0 : (parent.width - listBg.width - 1)
                        Behavior on width { NumberAnimation { duration: root.animColResize; easing.type: Easing.OutCubic } }
                        height: parent.height
                        color: Qt.rgba(1, 1, 1, 0.01)
                        clip: true

                        Loader {
                            anchors.fill: parent
                            anchors.margins: 20
                            anchors.topMargin: 24
                            active: panel.picker.visible && !panel.compact && panel.picker.selectedItem != null
                            sourceComponent: detailComp
                        }
                    }
                }
            }

        // Detail content — split out so the binding can short-circuit when the
        // panel is compact (Loader.active false).
        Component {
            id: detailComp
            Column {
                spacing: 0
                anchors.fill: parent

                // Project detail
                Column {
                    width: parent.width
                    spacing: 16
                    visible: panel.picker.selectedItem?.type === "project"
                    Column {
                        width: parent.width
                        spacing: 3
                        Text { text: "PATH"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                        Text {
                            text: {
                                const sel = panel.picker.selectedItem
                                if (!sel || !sel.path) return ""
                                const h = sel.host ? sel.host + ":" : ""
                                return h + sel.path.replace(/^\/home\/[^\/]+/, "~")
                            }
                            color: "#ccc"
                            font.pixelSize: 14
                            width: parent.width
                            elide: Text.ElideMiddle
                        }
                    }
                    Column {
                        width: parent.width
                        spacing: 6
                        visible: (panel.picker.selectedItem?.agents?.length || 0) > 0
                        Text { text: "AGENTS"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                        Repeater {
                            model: panel.picker.selectedItem?.agents || []
                            Rectangle {
                                id: agentRow
                                required property var modelData
                                readonly property color _phaseClr: panel.picker.phaseColor(modelData.phase)
                                width: parent.width
                                height: agentInner.implicitHeight + 6
                                radius: 6
                                color: "transparent"
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
                                            color: agentRow._phaseClr
                                        }
                                        Text {
                                            text: panel.picker.phaseLabel(agentRow.modelData.phase)
                                            color: agentRow._phaseClr
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
                    Column {
                        width: parent.width
                        spacing: 3
                        visible: (panel.picker.selectedItem?.launchers?.length || 0) > 0
                        Text { text: "LAUNCHERS"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                        Row {
                            spacing: 6
                            Repeater {
                                model: panel.picker.selectedItem?.launchers || []
                                Rectangle {
                                    required property string modelData
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
                    visible: panel.picker.selectedItem?.type === "workspace"
                    Text { text: "WORKSPACE"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                    Text { text: "Not yet tracked as a project."; color: "#888"; font.pixelSize: 14 }
                    Text { text: "Press Tab to promote"; color: "#666"; font.pixelSize: 13 }
                }

                // Agent detail
                Column {
                    width: parent.width
                    spacing: 16
                    visible: panel.picker.selectedItem?.type === "agent"
                    Column {
                        width: parent.width
                        spacing: 3
                        Text { text: "SESSION"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                        Text {
                            text: panel.picker.selectedItem?.sessionId || ""
                            color: "#ccc"
                            font.pixelSize: 13
                            font.family: "monospace"
                        }
                    }
                    Column {
                        width: parent.width
                        spacing: 3
                        visible: !!(panel.picker.selectedItem?.currentTool)
                        Text { text: "CURRENT TOOL"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                        Text {
                            text: panel.picker.selectedItem?.currentTool || ""
                            color: "#66BB6A"
                            font.pixelSize: 13
                            font.family: "monospace"
                        }
                    }
                    Column {
                        width: parent.width
                        spacing: 3
                        visible: !!(panel.picker.selectedItem?.host)
                        Text { text: "HOST"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                        Text { text: panel.picker.selectedItem?.host || ""; color: "#ccc"; font.pixelSize: 14 }
                    }
                    Column {
                        width: parent.width
                        spacing: 3
                        visible: !!(panel.picker.selectedItem?.lastPrompt)
                        Text { text: "LAST PROMPT"; color: "#555"; font.pixelSize: 11; font.weight: Font.Bold; font.letterSpacing: 1 }
                        Text {
                            text: panel.picker.selectedItem?.lastPrompt || ""
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
        }
    }

    Component {
        id: actionsComp
        PanelActions { picker: root }
    }

    /// Action list for the selected item. Pure display — keyboard is routed
    /// through the root-level searchInput, which dispatches by topPanel.kind.
    component PanelActions: Item {
        id: panel
        required property var picker
        property bool compact: false

        opacity: 0
        Component.onCompleted: opacity = 1
        Behavior on opacity { NumberAnimation { duration: root.animPanelFadeIn; easing.type: Easing.OutCubic } }

        ListView {
            id: actionListView
            anchors.fill: parent
            anchors.margins: 8
            clip: true
            boundsBehavior: Flickable.StopAtBounds
            model: panel.picker.actionList
            spacing: 2
            currentIndex: panel.picker.actionIndex
            onCurrentIndexChanged: positionViewAtIndex(currentIndex, ListView.Contain)

            delegate: Item {
                id: aItem
                required property var modelData
                required property int index
                width: actionListView.width
                height: modelData.section ? secText.implicitHeight : 32

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

                Rectangle {
                    visible: !aItem.modelData.section
                    anchors.fill: parent
                    radius: 8
                    color: panel.picker.actionIndex === aItem.index
                        ? Qt.rgba(1, 1, 1, 0.14)
                        : aItemMa.containsMouse ? Qt.rgba(1, 1, 1, 0.08) : "transparent"
                    Behavior on color { ColorAnimation { duration: root.animHover } }

                    MouseArea {
                        id: aItemMa
                        anchors.fill: parent
                        hoverEnabled: true
                        onClicked: { panel.picker.actionIndex = aItem.index; panel.picker.executeAction() }
                        onEntered: panel.picker.actionIndex = aItem.index
                    }

                    Row {
                        anchors.verticalCenter: parent.verticalCenter
                        anchors.left: parent.left
                        anchors.right: parent.right
                        anchors.margins: 10
                        spacing: 8

                        Text {
                            text: aItem.modelData.label || ""
                            color: panel.picker.actionIndex === aItem.index ? "#fff" : "#bbb"
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

    Component {
        id: promoteComp
        PanelPromote {
            picker: root
            wsId: root.topPanel.data ? (root.topPanel.data.wsId || 0) : 0
            wsRef: root.topPanel.data ? (root.topPanel.data.wsRef || "") : ""
            wsName: root.topPanel.data ? (root.topPanel.data.wsName || "") : ""
            seedName: root.topPanel.data ? (root.topPanel.data.seedName || "") : ""
        }
    }

    /// Promote-workspace wizard. Standalone — instantiated by the picker
    /// when a `promote` panel is on top of the stack.
    component PanelPromote: Item {
        id: panel
        required property var picker
        required property int wsId
        required property string wsRef    // arg for `niri msg action focus-workspace`
        required property string wsName   // current ws name in niri (empty for unnamed)
        property string seedName: ""
        property var availableLaunchers: []
        property var launchers: ({})
        property bool submitting: false
        // Project id conflicts → daemon will reject. Highlight in the form.
        readonly property bool nameConflicts: {
            const n = nameInput ? nameInput.text.trim() : ""
            if (!n) return false
            for (const p of panel.picker.projects || []) {
                if (p.id === n) return true
            }
            return false
        }

        opacity: 0
        Behavior on opacity { NumberAnimation { duration: root.animPanelFadeIn; easing.type: Easing.OutCubic } }
        Component.onCompleted: {
            opacity = 1
            nameInput.text = seedName
            if (wsId > 0) {
                ctxProc.command = [panel.picker.agora, "workspace-context", String(wsId)]
                ctxProc.running = true
            }
            nameInput.forceActiveFocus()
        }

        Process {
            id: ctxProc
            running: false
            stdout: StdioCollector {
                onStreamFinished: {
                    try {
                        const ctx = JSON.parse(text.trim() || "{}")
                        if (!nameInput.text) nameInput.text = ctx.suggested_name || ""
                        if (!pathInput.text) pathInput.text = ctx.suggested_path || ""
                        if (!hostInput.text) hostInput.text = ctx.host || ""
                        panel.availableLaunchers = ctx.available_launchers || []
                    } catch(e) { /* leave defaults */ }
                }
            }
        }

        function toggleLauncher(name) {
            const next = Object.assign({}, panel.launchers)
            next[name] = !next[name]
            panel.launchers = next
        }
        function shq(s) { return "'" + String(s).replace(/'/g, "'\\''") + "'" }

        function submit() {
            const name = nameInput.text.trim()
            const path = pathInput.text.trim()
            const host = hostInput.text.trim()
            if (!name || !path) return
            if (panel.nameConflicts) {
                panel.picker.reportError("Project '" + name + "' already exists. Pick a different name.")
                return
            }
            let cmd = "niri msg action focus-workspace " + shq(panel.wsRef)
            cmd += " && " + panel.picker.agora + " promote --name " + shq(name)
            if (host) cmd += " --host " + shq(host)
            for (const n of panel.availableLaunchers) {
                if (panel.launchers[n]) cmd += " --launcher " + shq(n)
            }
            // Rename whenever the new project name differs from the current
            // ws name (also covers the case where the ws was unnamed).
            if (name !== panel.wsName) cmd += " --rename-ws"
            cmd += " " + shq(path)
            panel.submitting = true
            submitProc.command = ["sh", "-c", cmd + "; exit $?"]
            submitProc.running = true
        }

        Process {
            id: submitProc
            running: false
            stdout: StdioCollector { id: submitOut }
            stderr: StdioCollector { id: submitErr }
            onExited: function(exitCode, exitStatus) {
                panel.submitting = false
                if (exitCode === 0) {
                    panel.picker.visible = false
                } else {
                    const raw = (submitErr.text || submitOut.text || "promote failed").trim()
                    panel.picker.reportError(raw.replace(/^Error:\s*daemon:\s*/, ""))
                }
            }
        }
        function cancel() { panel.picker.popPanel() }
        function keyHandler(event) {
            if (event.key === Qt.Key_Escape) {
                cancel(); event.accepted = true
            } else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) {
                submit(); event.accepted = true
            }
        }

        Column {
            anchors.fill: parent
            anchors.margins: 20
            anchors.topMargin: 24
            spacing: 14

            Text {
                text: "PROMOTE → " + (panel.wsName || panel.wsRef || ("ws " + panel.wsId))
                color: "#888"
                font.pixelSize: 11
                font.weight: Font.Bold
                font.letterSpacing: 1
            }

            // NAME
            Column {
                width: parent.width
                spacing: 4
                Text { text: "PROJECT NAME"; color: "#555"; font.pixelSize: 10; font.weight: Font.Bold; font.letterSpacing: 1 }
                Rectangle {
                    width: parent.width; height: 32; radius: 6
                    color: nameInput.activeFocus ? Qt.rgba(1,1,1,0.10) : Qt.rgba(1,1,1,0.05)
                    Behavior on color { ColorAnimation { duration: root.animTint } }
                    TextInput {
                        id: nameInput
                        anchors.fill: parent
                        anchors.leftMargin: 10; anchors.rightMargin: 10
                        verticalAlignment: TextInput.AlignVCenter
                        color: "#e0e0e0"; font.pixelSize: 14; clip: true
                        selectByMouse: true; selectionColor: Qt.rgba(0.4, 0.7, 1, 0.4)
                        KeyNavigation.tab: pathInput
                        Keys.onPressed: event => panel.keyHandler(event)
                    }
                }
            }

            // PATH
            Column {
                width: parent.width
                spacing: 4
                Text { text: "ROOT PATH"; color: "#555"; font.pixelSize: 10; font.weight: Font.Bold; font.letterSpacing: 1 }
                Rectangle {
                    width: parent.width; height: 32; radius: 6
                    color: pathInput.activeFocus ? Qt.rgba(1,1,1,0.10) : Qt.rgba(1,1,1,0.05)
                    Behavior on color { ColorAnimation { duration: root.animTint } }
                    TextInput {
                        id: pathInput
                        anchors.fill: parent
                        anchors.leftMargin: 10; anchors.rightMargin: 10
                        verticalAlignment: TextInput.AlignVCenter
                        color: "#e0e0e0"; font.pixelSize: 14; clip: true
                        selectByMouse: true; selectionColor: Qt.rgba(0.4, 0.7, 1, 0.4)
                        KeyNavigation.tab: hostInput
                        KeyNavigation.backtab: nameInput
                        Keys.onPressed: event => panel.keyHandler(event)
                    }
                }
            }

            // HOST
            Column {
                width: parent.width
                spacing: 4
                Text { text: "HOST  (empty = local)"; color: "#555"; font.pixelSize: 10; font.weight: Font.Bold; font.letterSpacing: 1 }
                Rectangle {
                    width: parent.width; height: 32; radius: 6
                    color: hostInput.activeFocus ? Qt.rgba(1,1,1,0.10) : Qt.rgba(1,1,1,0.05)
                    Behavior on color { ColorAnimation { duration: root.animTint } }
                    TextInput {
                        id: hostInput
                        anchors.fill: parent
                        anchors.leftMargin: 10; anchors.rightMargin: 10
                        verticalAlignment: TextInput.AlignVCenter
                        color: "#e0e0e0"; font.pixelSize: 14; clip: true
                        selectByMouse: true; selectionColor: Qt.rgba(0.4, 0.7, 1, 0.4)
                        KeyNavigation.backtab: pathInput
                        Keys.onPressed: event => panel.keyHandler(event)
                    }
                }
            }

            // LAUNCHERS
            Column {
                width: parent.width
                spacing: 4
                visible: panel.availableLaunchers.length > 0
                Text { text: "LAUNCHERS"; color: "#555"; font.pixelSize: 10; font.weight: Font.Bold; font.letterSpacing: 1 }
                Flow {
                    width: parent.width
                    spacing: 6
                    Repeater {
                        model: panel.availableLaunchers
                        Rectangle {
                            required property string modelData
                            property bool checked: !!panel.launchers[modelData]
                            width: lblText.implicitWidth + 28; height: 26; radius: 13
                            color: checked ? Qt.rgba(0.4, 0.7, 1, 0.25) : Qt.rgba(1, 1, 1, 0.06)
                            border.color: checked ? Qt.rgba(0.4, 0.7, 1, 0.6) : "transparent"
                            border.width: 1
                            Behavior on color { ColorAnimation { duration: root.animTint } }
                            Row {
                                anchors.centerIn: parent
                                spacing: 6
                                Text {
                                    text: parent.parent.checked ? "✓" : "○"
                                    color: parent.parent.checked ? "#7EC8E3" : "#666"
                                    font.pixelSize: 12
                                    anchors.verticalCenter: parent.verticalCenter
                                }
                                Text {
                                    id: lblText
                                    text: parent.parent.modelData
                                    color: parent.parent.checked ? "#e0e0e0" : "#aaa"
                                    font.pixelSize: 13
                                    anchors.verticalCenter: parent.verticalCenter
                                }
                            }
                            MouseArea {
                                anchors.fill: parent
                                cursorShape: Qt.PointingHandCursor
                                onClicked: panel.toggleLauncher(parent.modelData)
                            }
                        }
                    }
                }
            }

            // Submit / Cancel
            Row {
                anchors.right: parent.right
                spacing: 8
                Rectangle {
                    width: cancelText.implicitWidth + 24; height: 32; radius: 6
                    color: cancelMa.containsMouse ? Qt.rgba(1,1,1,0.08) : Qt.rgba(1,1,1,0.04)
                    Text {
                        id: cancelText
                        anchors.centerIn: parent
                        text: "Cancel  Esc"
                        color: "#888"; font.pixelSize: 13
                    }
                    MouseArea {
                        id: cancelMa
                        anchors.fill: parent; hoverEnabled: true
                        cursorShape: Qt.PointingHandCursor
                        onClicked: panel.cancel()
                    }
                }
                Rectangle {
                    property bool canSubmit: nameInput.text.trim().length > 0
                        && pathInput.text.trim().length > 0
                        && !panel.nameConflicts
                        && !panel.submitting
                    width: submitText.implicitWidth + 24; height: 32; radius: 6
                    color: !canSubmit ? Qt.rgba(1,1,1,0.04)
                          : submitMa.containsMouse ? Qt.rgba(0.4, 0.7, 1, 0.4)
                          : Qt.rgba(0.4, 0.7, 1, 0.28)
                    Behavior on color { ColorAnimation { duration: root.animTint } }
                    Text {
                        id: submitText
                        anchors.centerIn: parent
                        text: panel.submitting ? "…" : "Promote  ↵"
                        color: parent.canSubmit ? "#fff" : "#555"
                        font.pixelSize: 13
                        font.weight: Font.Medium
                    }
                    MouseArea {
                        id: submitMa
                        anchors.fill: parent; hoverEnabled: true
                        cursorShape: parent.canSubmit ? Qt.PointingHandCursor : Qt.ArrowCursor
                        onClicked: if (parent.canSubmit) panel.submit()
                    }
                }
            }
        }
    }
}
