import QtQuick
import QtQuick.Effects
import Quickshell
import Quickshell.Io
import Quickshell.Widgets
import qs.Common
import qs.Services
import qs.Widgets

Item {
    id: root

    property bool isVertical: axis?.isVertical ?? false
    property var axis: null
    property string screenName: ""
    property real widgetHeight: 30
    property real barThickness: 48
    property var barConfig: null

    // Agora state polling (single IPC call)
    property var agentsByWorkspace: ({})
    property var projectWsNames: new Set()

    Process {
        id: stateProc
        command: ["/home/jay/.local/bin/agora", "picker-state"]
        stdout: SplitParser {
            splitMarker: ""
            onRead: data => {
                try {
                    const d = JSON.parse(data);
                    const ps = d.PickerState || d;
                    const prio = {"waiting_permission": 4, "waiting_input": 3, "running": 2, "idle": 1};
                    const result = {};
                    for (const a of (ps.agents || [])) {
                        const ws = a.workspace || a.project || "";
                        const phase = a.phase || "idle";
                        if (ws && (!result[ws] || (prio[phase] || 0) > (prio[result[ws]] || 0)))
                            result[ws] = phase;
                    }
                    root.agentsByWorkspace = result;
                    const names = new Set();
                    for (const p of (ps.projects || [])) names.add(p.workspace_name);
                    root.projectWsNames = names;
                } catch (e) {
                    root.agentsByWorkspace = {};
                    root.projectWsNames = new Set();
                }
            }
        }
    }

    Timer {
        interval: 3000
        running: true
        repeat: true
        triggeredOnStart: true
        onTriggered: {
            if (!stateProc.running) stateProc.running = true;
        }
    }

    function isProjectWorkspace(wsName) {
        return wsName && root.projectWsNames.has(wsName);
    }

    function agentPhaseForWs(wsName) {
        if (!wsName) return "";
        return root.agentsByWorkspace[wsName] || "";
    }

    function agentDotColor(phase) {
        switch (phase) {
        case "waiting_permission": return "#FFA726";
        case "waiting_input": return "#EF5350";
        case "running": return "#66BB6A";
        default: return "transparent";
        }
    }

    property var blurBarWindow: null
    property var parentScreen: null

    readonly property real _leftMargin: {
        if (isVertical)
            return 0;
        root.x;
        if (!root.parent)
            return 0;
        const gap = root.mapToItem(null, 0, 0).x;
        return (gap > 0 && gap < 30) ? gap + 5 : 0;
    }
    readonly property real _rightMargin: {
        if (isVertical)
            return 0;
        root.x;
        root.width;
        if (!root.parent || !blurBarWindow)
            return 0;
        const gap = blurBarWindow.width - root.mapToItem(null, root.width, 0).x;
        return (gap > 0 && gap < 30) ? gap + 5 : 0;
    }
    readonly property real _topMargin: {
        if (!isVertical)
            return 0;
        root.y;
        if (!root.parent)
            return 0;
        const gap = root.mapToItem(null, 0, 0).y;
        return (gap > 0 && gap < 30) ? gap + 5 : 0;
    }
    readonly property real _bottomMargin: {
        if (!isVertical)
            return 0;
        root.y;
        root.height;
        if (!root.parent || !blurBarWindow)
            return 0;
        const gap = blurBarWindow.height - root.mapToItem(null, 0, root.height).y;
        return (gap > 0 && gap < 30) ? gap + 5 : 0;
    }

    property int _desktopEntriesUpdateTrigger: 0
    readonly property var sortedToplevels: {
        return CompositorService.filterCurrentWorkspace(CompositorService.sortedToplevels, screenName);
    }

    readonly property string effectiveScreenName: {
        if (!SettingsData.workspaceFollowFocus)
            return root.screenName;
        return NiriService.currentOutput || root.screenName;
    }

    Connections {
        target: DesktopEntries
        function onApplicationsChanged() {
            _desktopEntriesUpdateTrigger++;
        }
    }

    property var currentWorkspace: getNiriActiveWorkspace()

    property var workspaceList: {
        const baseList = getNiriWorkspaces();
        return SettingsData.showWorkspacePadding ? padWorkspaces(baseList) : baseList;
    }

    function getWorkspaceIcons(ws) {
        _desktopEntriesUpdateTrigger;
        if (!SettingsData.showWorkspaceApps || !ws) {
            return [];
        }

        let targetWorkspaceId;
        if (!ws || typeof ws !== "object") {
            const wsNumber = typeof ws === "number" ? ws : -1;
            if (wsNumber <= 0) {
                return [];
            }
            const workspace = NiriService.allWorkspaces.find(w => w.idx + 1 === wsNumber && w.output === root.effectiveScreenName);
            if (!workspace) {
                return [];
            }
            targetWorkspaceId = workspace.id;
        } else {
            if (ws.id === undefined || ws.id === -1 || ws.idx === -1) {
                return [];
            }
            targetWorkspaceId = ws.id;
        }

        const wins = NiriService.windows || [];

        const byApp = {};
        const isActiveWs = NiriService.allWorkspaces.some(ws => ws.id === targetWorkspaceId && ws.is_active);

        wins.forEach((w, i) => {
            if (!w)
                return;

            if (w.workspace_id !== targetWorkspaceId)
                return;

            const keyBase = (w.app_id || w.appId || w.class || w.windowClass || "unknown");
            const moddedId = Paths.moddedAppId(keyBase);
            const key = isActiveWs || !SettingsData.groupWorkspaceApps ? `${moddedId}_${i}` : moddedId;

            if (!byApp[key]) {
                const isQuickshell = keyBase === "org.quickshell" || keyBase === "com.danklinux.dms";
                const isSteamApp = Paths.isSteamApp(moddedId);
                const desktopEntry = DesktopEntries.heuristicLookup(moddedId);
                const icon = Paths.getAppIcon(moddedId, desktopEntry);
                const appName = Paths.getAppName(moddedId, desktopEntry);
                byApp[key] = {
                    "type": "icon",
                    "icon": icon,
                    "isQuickshell": isQuickshell,
                    "isSteamApp": isSteamApp,
                    "active": !!(w.is_focused),
                    "count": 1,
                    "windowId": w.address || w.id,
                    "fallbackText": appName || ""
                };
            } else {
                byApp[key].count++;
                if (w.is_focused) {
                    byApp[key].active = true;
                }
            }
        });

        return Object.values(byApp);
    }

    function padWorkspaces(list) {
        const padded = list.slice();
        const placeholder = {
            "id": -1,
            "idx": -1,
            "name": ""
        };
        while (padded.length < 3) {
            padded.push(placeholder);
        }
        return padded;
    }

    function getNiriWorkspaces() {
        if (NiriService.allWorkspaces.length === 0) {
            return [
                {
                    "id": 1,
                    "idx": 0,
                    "name": ""
                },
                {
                    "id": 2,
                    "idx": 1,
                    "name": ""
                }
            ];
        }

        const fallbackWorkspaces = [
            {
                "id": 1,
                "idx": 0,
                "name": ""
            },
            {
                "id": 2,
                "idx": 1,
                "name": ""
            }
        ];

        let workspaces;
        if (!root.screenName || SettingsData.workspaceFollowFocus) {
            const currentWorkspaces = NiriService.getCurrentOutputWorkspaces();
            workspaces = currentWorkspaces.length > 0 ? currentWorkspaces : fallbackWorkspaces;
        } else {
            const displayWorkspaces = NiriService.allWorkspaces.filter(ws => ws.output === root.screenName);
            workspaces = displayWorkspaces.length > 0 ? displayWorkspaces : fallbackWorkspaces;
        }

        workspaces = workspaces.slice().sort((a, b) => a.idx - b.idx);

        if (!SettingsData.showOccupiedWorkspacesOnly) {
            return workspaces;
        }

        return workspaces.filter(ws => {
            if (ws.is_active)
                return true;
            return NiriService.windows?.some(win => win.workspace_id === ws.id) ?? false;
        });
    }

    function getNiriActiveWorkspace() {
        if (NiriService.allWorkspaces.length === 0) {
            return 1;
        }

        if (!root.screenName || SettingsData.workspaceFollowFocus) {
            return NiriService.getCurrentWorkspaceNumber();
        }

        const activeWs = NiriService.allWorkspaces.find(ws => ws.output === root.screenName && ws.is_active);
        return activeWs ? activeWs.idx : 1;
    }

    readonly property real dpr: parentScreen ? CompositorService.getScreenScale(parentScreen) : 1
    readonly property real padding: (root.barConfig?.removeWidgetPadding ?? false) ? 0 : Theme.snap((root.barConfig?.widgetPadding ?? 12) * (widgetHeight / 30), dpr)
    readonly property real visualWidth: isVertical ? widgetHeight : (workspaceRow.implicitWidth + padding * 2)
    readonly property real visualHeight: isVertical ? (workspaceRow.implicitHeight + padding * 2) : widgetHeight
    readonly property real appIconSize: Theme.barIconSize(barThickness, -6 + SettingsData.workspaceAppIconSizeOffset, root.barConfig?.maximizeWidgetIcons, root.barConfig?.iconScale)

    function getRealWorkspaces() {
        return root.workspaceList.filter(ws => ws && ws.idx !== -1);
    }

    function switchToWorkspaceByModelData(data) {
        if (!data)
            return;
        if (data.idx !== undefined)
            NiriService.switchToWorkspace(data.idx);
    }

    function findClosestWorkspaceIndex(localX, localY) {
        if (workspaceRepeater.count === 0)
            return -1;

        let closestIdx = -1;
        let closestDist = Infinity;

        for (let i = 0; i < workspaceRepeater.count; i++) {
            const item = workspaceRepeater.itemAt(i);
            if (!item)
                continue;
            const center = item.mapToItem(root, item.width / 2, item.height / 2);
            const dist = isVertical ? Math.abs(localY - center.y) : Math.abs(localX - center.x);
            if (dist < closestDist) {
                closestDist = dist;
                closestIdx = i;
            }
        }
        return closestIdx;
    }

    function switchWorkspace(direction) {
        const realWorkspaces = getRealWorkspaces();
        if (realWorkspaces.length < 2)
            return;

        const currentIndex = realWorkspaces.findIndex(ws => ws && ws.idx === root.currentWorkspace);
        const validIndex = currentIndex === -1 ? 0 : currentIndex;
        const nextIndex = direction > 0 ? Math.min(validIndex + 1, realWorkspaces.length - 1) : Math.max(validIndex - 1, 0);

        if (nextIndex === validIndex)
            return;

        const nextWorkspace = realWorkspaces[nextIndex];
        if (!nextWorkspace || nextWorkspace.idx === undefined)
            return;
        NiriService.switchToWorkspace(nextWorkspace.idx);
    }

    function getWorkspaceIndexFallback(modelData, index) {
        return (modelData?.idx !== undefined && modelData?.idx !== -1) ? modelData.idx : "";
    }

    function getWorkspaceIndex(modelData, index) {
        const isPlaceholder = modelData?.idx === -1;

        if (isPlaceholder)
            return index + 1;

        let workspaceName = "";
        if (SettingsData.showWorkspaceName) {
            workspaceName = modelData?.name ?? "";

            if (workspaceName && workspaceName !== "") {
                if (root.isVertical) {
                    workspaceName = workspaceName.charAt(0);
                }
            } else {
                workspaceName = "";
            }
        }

        if (workspaceName) {
            if (SettingsData.showWorkspaceIndex) {
                const indexLabel = getWorkspaceIndexFallback(modelData, index);
                return indexLabel ? `${indexLabel}: ${workspaceName}` : workspaceName;
            }
            return workspaceName;
        }

        return getWorkspaceIndexFallback(modelData, index);
    }

    readonly property bool hasWorkspaces: getRealWorkspaces().length > 0

    width: hasWorkspaces ? (isVertical ? barThickness : visualWidth) : 0
    height: hasWorkspaces ? (isVertical ? visualHeight : barThickness) : 0
    visible: hasWorkspaces

    Item {
        id: visualBackground
        width: root.visualWidth
        height: root.visualHeight
        anchors.centerIn: parent

        Rectangle {
            id: outline
            anchors.centerIn: parent
            width: {
                const borderWidth = (barConfig?.widgetOutlineEnabled ?? false) ? (barConfig?.widgetOutlineThickness ?? 1) : 0;
                return parent.width + borderWidth * 2;
            }
            height: {
                const borderWidth = (barConfig?.widgetOutlineEnabled ?? false) ? (barConfig?.widgetOutlineThickness ?? 1) : 0;
                return parent.height + borderWidth * 2;
            }
            radius: (barConfig?.noBackground ?? false) ? 0 : Theme.cornerRadius
            color: "transparent"
            border.width: {
                if (barConfig?.widgetOutlineEnabled ?? false) {
                    return barConfig?.widgetOutlineThickness ?? 1;
                }
                return 0;
            }
            border.color: {
                if (!(barConfig?.widgetOutlineEnabled ?? false)) {
                    return "transparent";
                }
                const colorOption = barConfig?.widgetOutlineColor || "primary";
                const opacity = barConfig?.widgetOutlineOpacity ?? 1.0;
                switch (colorOption) {
                case "surfaceText":
                    return Theme.withAlpha(Theme.surfaceText, opacity);
                case "secondary":
                    return Theme.withAlpha(Theme.secondary, opacity);
                case "primary":
                    return Theme.withAlpha(Theme.primary, opacity);
                default:
                    return Theme.withAlpha(Theme.primary, opacity);
                }
            }
        }

        Rectangle {
            id: background
            anchors.fill: parent
            radius: (barConfig?.noBackground ?? false) ? 0 : Theme.cornerRadius
            color: {
                if ((barConfig?.noBackground ?? false))
                    return "transparent";
                const baseColor = Theme.widgetBaseBackgroundColor;
                const transparency = (root.barConfig && root.barConfig.widgetTransparency !== undefined) ? root.barConfig.widgetTransparency : 1.0;
                if (Theme.widgetBackgroundHasAlpha) {
                    return Qt.rgba(baseColor.r, baseColor.g, baseColor.b, baseColor.a * transparency);
                }
                return Theme.withAlpha(baseColor, transparency);
            }
        }
    }

    MouseArea {
        id: edgeMouseArea
        z: -1
        x: -root._leftMargin
        y: -root._topMargin
        width: root.width + root._leftMargin + root._rightMargin
        height: root.height + root._topMargin + root._bottomMargin
        acceptedButtons: Qt.LeftButton | Qt.RightButton
        hoverEnabled: true
        cursorShape: Qt.PointingHandCursor

        property real touchpadAccumulator: 0
        property real mouseAccumulator: 0
        property bool scrollInProgress: false

        Timer {
            id: scrollCooldown
            interval: 100
            onTriggered: parent.scrollInProgress = false
        }

        onClicked: mouse => {
            const rootPos = edgeMouseArea.mapToItem(root, mouse.x, mouse.y);
            switch (mouse.button) {
            case Qt.RightButton:
                NiriService.toggleOverview();
                break;
            case Qt.LeftButton:
                const idx = root.findClosestWorkspaceIndex(rootPos.x, rootPos.y);
                if (idx >= 0)
                    root.switchToWorkspaceByModelData(root.workspaceList[idx]);
                break;
            }
        }

        onWheel: wheel => {
            if (Math.abs(wheel.angleDelta.x) > Math.abs(wheel.angleDelta.y)) {
                wheel.accepted = false;
                return;
            }

            if (scrollInProgress)
                return;

            const delta = wheel.angleDelta.y;
            const isTouchpad = wheel.pixelDelta && wheel.pixelDelta.y !== 0;
            const reverse = SettingsData.reverseScrolling ? -1 : 1;

            if (isTouchpad) {
                touchpadAccumulator += delta;
                if (Math.abs(touchpadAccumulator) < 500)
                    return;
                const direction = touchpadAccumulator * reverse < 0 ? 1 : -1;
                root.switchWorkspace(direction);
                scrollInProgress = true;
                scrollCooldown.restart();
                touchpadAccumulator = 0;
                return;
            }

            mouseAccumulator += delta;
            if (Math.abs(mouseAccumulator) < 120)
                return;
            const direction = mouseAccumulator * reverse < 0 ? 1 : -1;
            root.switchWorkspace(direction);
            scrollInProgress = true;
            scrollCooldown.restart();
            mouseAccumulator = 0;
        }
    }

    property int dragSourceIndex: -1
    property int dragTargetIndex: -1
    property bool suppressShiftAnimation: false

    onWorkspaceListChanged: {
        if (dragSourceIndex >= 0) {
            dragSourceIndex = -1;
            dragTargetIndex = -1;
            suppressShiftAnimation = false;
        }
    }

    Flow {
        id: workspaceRow

        x: isVertical ? visualBackground.x : (parent.width - implicitWidth) / 2
        y: isVertical ? (parent.height - implicitHeight) / 2 : visualBackground.y
        spacing: Theme.spacingS
        flow: isVertical ? Flow.TopToBottom : Flow.LeftToRight

        Repeater {
            id: workspaceRepeater
            model: ScriptModel {
                values: root.workspaceList
            }

            Item {
                id: delegateRoot

                property bool isDropTarget: root.dragTargetIndex === index

                z: dragHandler.dragging ? 1000 : 1

                property real shiftOffset: {
                    if (root.dragSourceIndex < 0 || index === root.dragSourceIndex)
                        return 0;
                    const dragIdx = root.dragSourceIndex;
                    const dropIdx = root.dragTargetIndex;
                    if (dropIdx < 0)
                        return 0;
                    const shiftAmount = delegateRoot.width + Theme.spacingS;
                    if (dragIdx < dropIdx && index > dragIdx && index <= dropIdx)
                        return -shiftAmount;
                    if (dragIdx > dropIdx && index >= dropIdx && index < dragIdx)
                        return shiftAmount;
                    return 0;
                }

                transform: Translate {
                    x: root.isVertical ? 0 : delegateRoot.shiftOffset
                    y: root.isVertical ? delegateRoot.shiftOffset : 0
                    Behavior on x {
                        enabled: !root.suppressShiftAnimation
                        NumberAnimation {
                            duration: 150
                            easing.type: Easing.OutCubic
                        }
                    }
                    Behavior on y {
                        enabled: !root.suppressShiftAnimation
                        NumberAnimation {
                            duration: 150
                            easing.type: Easing.OutCubic
                        }
                    }
                }

                property string _prevAgentPhase: ""
                property string _currentAgentPhase: root.agentPhaseForWs(modelData?.name)
                on_CurrentAgentPhaseChanged: {
                    if (_prevAgentPhase !== "" && _currentAgentPhase !== _prevAgentPhase && _currentAgentPhase !== "") {
                        phaseFlash.color = root.agentDotColor(_currentAgentPhase)
                        phaseFlash.opacity = 0.35
                        phaseFlashAnim.restart()
                    }
                    _prevAgentPhase = _currentAgentPhase
                }

                property bool isActive: !!(modelData && modelData.idx === root.currentWorkspace)
                property bool isOccupied: {
                    const workspace = NiriService.allWorkspaces.find(ws => ws.idx + 1 === modelData && ws.output === root.effectiveScreenName);
                    return workspace ? (NiriService.windows?.some(win => win.workspace_id === workspace.id) ?? false) : false;
                }
                property bool isPlaceholder: !!(modelData && modelData.idx === -1)
                property bool isHovered: mouseArea.containsMouse

                property var loadedWorkspaceData: null
                property bool loadedIsUrgent: false
                property bool isUrgent: loadedIsUrgent
                readonly property var loadedIconData: {
                    if (isPlaceholder)
                        return null;
                    const name = modelData?.name;
                    if (!name)
                        return null;
                    return SettingsData.getWorkspaceNameIcon(name);
                }
                readonly property bool loadedHasIcon: loadedIconData !== null
                property var loadedIcons: []

                readonly property int stableIconCount: {
                    if (!SettingsData.showWorkspaceApps || isPlaceholder)
                        return 0;

                    const targetWorkspaceId = modelData?.id;
                    if (targetWorkspaceId === undefined || targetWorkspaceId === null)
                        return 0;

                    const wins = NiriService.windows || [];
                    const seen = {};
                    let groupedCount = 0;
                    let totalCount = 0;

                    for (let i = 0; i < wins.length; i++) {
                        const w = wins[i];
                        if (!w)
                            continue;

                        if (w.workspace_id !== targetWorkspaceId)
                            continue;
                        totalCount++;

                        const appKey = w.app_id || w.appId || w.class || w.windowClass || "unknown";
                        if (!seen[appKey]) {
                            seen[appKey] = true;
                            groupedCount++;
                        }
                    }

                    return (SettingsData.groupWorkspaceApps && !isActive) ? groupedCount : totalCount;
                }

                readonly property real baseWidth: root.isVertical ? (SettingsData.showWorkspaceApps ? Math.max(widgetHeight * 0.7, root.appIconSize + Theme.spacingXS * 2) : widgetHeight * 0.5) : (isActive ? Math.max(root.widgetHeight * 1.05, root.appIconSize * 1.6) : Math.max(root.widgetHeight * 0.7, root.appIconSize * 1.2))
                readonly property real baseHeight: root.isVertical ? (isActive ? Math.max(root.widgetHeight * 1.05, root.appIconSize * 1.6) : Math.max(root.widgetHeight * 0.7, root.appIconSize * 1.2)) : (SettingsData.showWorkspaceApps ? Math.max(widgetHeight * 0.7, root.appIconSize + Theme.spacingXS * 2) : widgetHeight * 0.5)
                readonly property bool hasWorkspaceName: SettingsData.showWorkspaceName && modelData?.name && modelData.name !== ""
                readonly property bool workspaceNamesEnabled: SettingsData.showWorkspaceName
                readonly property real contentImplicitWidth: appIconsLoader.item?.contentWidth ?? 0
                readonly property real contentImplicitHeight: appIconsLoader.item?.contentHeight ?? 0

                readonly property real iconsExtraWidth: {
                    if (!root.isVertical && SettingsData.showWorkspaceApps && stableIconCount > 0) {
                        const numIcons = Math.min(stableIconCount, SettingsData.maxWorkspaceIcons);
                        return numIcons * root.appIconSize + (numIcons > 0 ? (numIcons - 1) * Theme.spacingXS : 0) + (isActive ? Theme.spacingXS : 0);
                    }
                    return 0;
                }
                readonly property real iconsExtraHeight: {
                    if (root.isVertical && SettingsData.showWorkspaceApps && stableIconCount > 0) {
                        const numIcons = Math.min(stableIconCount, SettingsData.maxWorkspaceIcons);
                        return numIcons * root.appIconSize + (numIcons > 0 ? (numIcons - 1) * Theme.spacingXS : 0) + (isActive ? Theme.spacingXS : 0);
                    }
                    return 0;
                }

                readonly property real visualWidth: {
                    if (contentImplicitWidth <= 0)
                        return baseWidth + iconsExtraWidth;
                    const padding = root.isVertical ? Theme.spacingXS : Theme.spacingS;
                    return Math.max(baseWidth + iconsExtraWidth, contentImplicitWidth + padding);
                }
                readonly property real visualHeight: {
                    if (contentImplicitHeight <= 0)
                        return baseHeight + iconsExtraHeight;
                    const padding = root.isVertical ? Theme.spacingS : Theme.spacingXS;
                    return Math.max(baseHeight + iconsExtraHeight, contentImplicitHeight + padding);
                }

                readonly property color unfocusedColor: {
                    switch (SettingsData.workspaceUnfocusedColorMode) {
                    case "s":
                        return Theme.surface;
                    case "sc":
                        return Theme.surfaceContainer;
                    case "sch":
                        return Theme.surfaceContainerHigh;
                    default:
                        return Theme.surfaceTextAlpha;
                    }
                }

                readonly property color activeColor: {
                    switch (SettingsData.workspaceColorMode) {
                    case "s":
                        return Theme.surface;
                    case "sc":
                        return Theme.surfaceContainer;
                    case "sch":
                        return Theme.surfaceContainerHigh;
                    case "none":
                        return unfocusedColor;
                    default:
                        return Theme.primary;
                    }
                }

                readonly property color occupiedColor: {
                    switch (SettingsData.workspaceOccupiedColorMode) {
                    case "sec":
                        return Theme.secondary;
                    case "s":
                        return Theme.surface;
                    case "sc":
                        return Theme.surfaceContainer;
                    case "sch":
                        return Theme.surfaceContainerHigh;
                    case "schh":
                        return Theme.surfaceContainerHighest;
                    default:
                        return unfocusedColor;
                    }
                }

                readonly property color urgentColor: {
                    switch (SettingsData.workspaceUrgentColorMode) {
                    case "primary":
                        return Theme.primary;
                    case "secondary":
                        return Theme.secondary;
                    case "s":
                        return Theme.surface;
                    case "sc":
                        return Theme.surfaceContainer;
                    default:
                        return Theme.error;
                    }
                }

                readonly property color focusedBorderColor: {
                    switch (SettingsData.workspaceFocusedBorderColor) {
                    case "surfaceText":
                        return Theme.surfaceText;
                    case "secondary":
                        return Theme.secondary;
                    default:
                        return Theme.primary;
                    }
                }

                function getContrastingIconColor(bgColor) {
                    const luminance = 0.299 * bgColor.r + 0.587 * bgColor.g + 0.114 * bgColor.b;
                    return luminance > 0.4 ? Qt.rgba(0.15, 0.15, 0.15, 1) : Qt.rgba(0.8, 0.8, 0.8, 1);
                }

                readonly property color quickshellIconActiveColor: getContrastingIconColor(activeColor)
                readonly property color quickshellIconInactiveColor: getContrastingIconColor(unfocusedColor)

                Item {
                    id: dragHandler
                    anchors.fill: parent
                    property bool dragging: false
                    property point dragStartPos: Qt.point(0, 0)
                    property real dragAxisOffset: 0

                    Connections {
                        target: root
                        function onWorkspaceListChanged() {
                            if (dragHandler.dragging) {
                                dragHandler.dragging = false;
                                dragHandler.dragAxisOffset = 0;
                                mouseArea.mousePressed = false;
                            }
                        }
                    }
                }

                MouseArea {
                    id: mouseArea
                    anchors.fill: parent
                    hoverEnabled: !isPlaceholder
                    cursorShape: isPlaceholder ? Qt.ArrowCursor : (dragHandler.dragging ? Qt.ClosedHandCursor : Qt.PointingHandCursor)
                    enabled: !isPlaceholder
                    acceptedButtons: Qt.LeftButton | Qt.RightButton

                    property bool mousePressed: false

                    onPressed: mouse => {
                        if (mouse.button === Qt.LeftButton && SettingsData.workspaceDragReorder && !isPlaceholder) {
                            mousePressed = true;
                            dragHandler.dragStartPos = Qt.point(mouse.x, mouse.y);
                        }
                    }

                    onPositionChanged: mouse => {
                        if (!mousePressed || !SettingsData.workspaceDragReorder || isPlaceholder)
                            return;

                        if (!dragHandler.dragging) {
                            const distance = root.isVertical ? Math.abs(mouse.y - dragHandler.dragStartPos.y) : Math.abs(mouse.x - dragHandler.dragStartPos.x);
                            if (distance > 5) {
                                dragHandler.dragging = true;
                                root.dragSourceIndex = index;
                                root.dragTargetIndex = index;
                            }
                        }

                        if (!dragHandler.dragging)
                            return;

                        const rawAxisOffset = root.isVertical ? (mouse.y - dragHandler.dragStartPos.y) : (mouse.x - dragHandler.dragStartPos.x);

                        const itemSize = (root.isVertical ? delegateRoot.height : delegateRoot.width) + Theme.spacingS;
                        const maxOffsetPositive = (root.workspaceList.length - 1 - index) * itemSize;
                        const maxOffsetNegative = -index * itemSize;
                        const axisOffset = Math.max(maxOffsetNegative, Math.min(maxOffsetPositive, rawAxisOffset));
                        dragHandler.dragAxisOffset = axisOffset;

                        const slotOffset = Math.round(axisOffset / itemSize);
                        const newTargetIndex = Math.max(0, Math.min(root.workspaceList.length - 1, index + slotOffset));

                        if (newTargetIndex !== root.dragTargetIndex) {
                            root.dragTargetIndex = newTargetIndex;
                        }
                    }

                    onReleased: mouse => {
                        const wasDragging = dragHandler.dragging;
                        const didReorder = wasDragging && root.dragTargetIndex >= 0 && root.dragTargetIndex !== root.dragSourceIndex;

                        if (didReorder) {
                            const sourceWs = root.workspaceList[root.dragSourceIndex];
                            const targetWs = root.workspaceList[root.dragTargetIndex];

                            if (sourceWs && targetWs && sourceWs.idx !== undefined && targetWs.idx !== undefined) {
                                root.suppressShiftAnimation = true;
                                NiriService.moveWorkspaceToIndex(sourceWs.idx, targetWs.idx);
                                Qt.callLater(() => root.suppressShiftAnimation = false);
                            }
                        }

                        mousePressed = false;
                        dragHandler.dragging = false;
                        dragHandler.dragAxisOffset = 0;
                        root.dragSourceIndex = -1;
                        root.dragTargetIndex = -1;

                        if (wasDragging || isPlaceholder)
                            return;

                        if (mouse.button === Qt.LeftButton) {
                            if (modelData && modelData.idx !== undefined) {
                                NiriService.switchToWorkspace(modelData.idx);
                            }
                        } else if (mouse.button === Qt.RightButton) {
                            NiriService.toggleOverview();
                        }
                    }
                }

                Timer {
                    id: dataUpdateTimer
                    interval: 50
                    onTriggered: {
                        if (isPlaceholder) {
                            delegateRoot.loadedWorkspaceData = null;
                            delegateRoot.loadedIcons = [];
                            delegateRoot.loadedIsUrgent = false;
                            return;
                        }

                        delegateRoot.loadedWorkspaceData = modelData || null;
                        const workspaceId = modelData?.id;
                        delegateRoot.loadedIsUrgent = workspaceId ? NiriService.windows.some(w => w.workspace_id === workspaceId && w.is_urgent) : false;

                        if (SettingsData.showWorkspaceApps) {
                            delegateRoot.loadedIcons = root.getWorkspaceIcons(isPlaceholder ? null : modelData);
                        } else {
                            delegateRoot.loadedIcons = [];
                        }
                    }
                }

                function updateAllData() {
                    dataUpdateTimer.restart();
                }

                width: root.isVertical ? root.widgetHeight : visualWidth
                height: root.isVertical ? visualHeight : root.widgetHeight

                Behavior on width {
                    NumberAnimation {
                        duration: Theme.mediumDuration
                        easing.type: Theme.emphasizedEasing
                    }
                }

                Behavior on height {
                    NumberAnimation {
                        duration: Theme.mediumDuration
                        easing.type: Theme.emphasizedEasing
                    }
                }

                Rectangle {
                    id: focusedBorderRing
                    x: root.isVertical ? (root.widgetHeight - width) / 2 : (parent.width - width) / 2
                    y: root.isVertical ? (parent.height - height) / 2 : (root.widgetHeight - height) / 2
                    width: {
                        const borderWidth = (SettingsData.workspaceFocusedBorderEnabled && isActive && !isPlaceholder) ? SettingsData.workspaceFocusedBorderThickness : 0;
                        return delegateRoot.visualWidth + borderWidth * 2;
                    }
                    height: {
                        const borderWidth = (SettingsData.workspaceFocusedBorderEnabled && isActive && !isPlaceholder) ? SettingsData.workspaceFocusedBorderThickness : 0;
                        return delegateRoot.visualHeight + borderWidth * 2;
                    }
                    radius: Theme.cornerRadius
                    color: "transparent"
                    border.width: (SettingsData.workspaceFocusedBorderEnabled && isActive && !isPlaceholder) ? SettingsData.workspaceFocusedBorderThickness : 0
                    border.color: (SettingsData.workspaceFocusedBorderEnabled && isActive && !isPlaceholder) ? focusedBorderColor : "transparent"

                    Behavior on width {
                        NumberAnimation {
                            duration: Theme.mediumDuration
                            easing.type: Theme.emphasizedEasing
                        }
                    }

                    Behavior on height {
                        NumberAnimation {
                            duration: Theme.mediumDuration
                            easing.type: Theme.emphasizedEasing
                        }
                    }

                    Behavior on border.width {
                        NumberAnimation {
                            duration: Theme.mediumDuration
                            easing.type: Theme.emphasizedEasing
                        }
                    }

                    Behavior on border.color {
                        ColorAnimation {
                            duration: Theme.mediumDuration
                            easing.type: Theme.emphasizedEasing
                        }
                    }
                }

                Rectangle {
                    id: visualContent
                    width: delegateRoot.visualWidth
                    height: delegateRoot.visualHeight
                    x: root.isVertical ? (root.widgetHeight - width) / 2 : (parent.width - width) / 2
                    y: root.isVertical ? (parent.height - height) / 2 : (root.widgetHeight - height) / 2
                    radius: Theme.cornerRadius
                    color: isActive ? activeColor : isUrgent ? urgentColor : isPlaceholder ? Theme.surfaceTextLight : isHovered ? Theme.withAlpha(unfocusedColor, 0.7) : isOccupied ? occupiedColor : unfocusedColor
                    opacity: dragHandler.dragging ? 0.8 : 1.0

                    border.width: dragHandler.dragging ? 2 : (isUrgent ? 2 : (isDropTarget ? 2 : (root.isProjectWorkspace(modelData?.name) ? 1.5 : 0)))
                    border.color: {
                        if (dragHandler.dragging || isDropTarget) return Theme.primary;
                        if (isUrgent) return urgentColor;
                        if (root.isProjectWorkspace(modelData?.name)) {
                            const phase = root.agentPhaseForWs(modelData?.name);
                            if (phase && phase !== "idle") return root.agentDotColor(phase);
                            return Theme.withAlpha(Theme.primary, 0.6);
                        }
                        return "transparent";
                    }

                    transform: Translate {
                        x: root.isVertical ? 0 : (dragHandler.dragging ? dragHandler.dragAxisOffset : 0)
                        y: root.isVertical ? (dragHandler.dragging ? dragHandler.dragAxisOffset : 0) : 0
                    }

                    Behavior on opacity {
                        NumberAnimation {
                            duration: Theme.shortDuration
                            easing.type: Theme.emphasizedEasing
                        }
                    }

                    Behavior on width {
                        NumberAnimation {
                            duration: Theme.mediumDuration
                            easing.type: Theme.emphasizedEasing
                        }
                    }

                    Behavior on height {
                        NumberAnimation {
                            duration: Theme.mediumDuration
                            easing.type: Theme.emphasizedEasing
                        }
                    }

                    Behavior on color {
                        ColorAnimation {
                            duration: Theme.mediumDuration
                            easing.type: Theme.emphasizedEasing
                        }
                    }

                    Behavior on border.width {
                        NumberAnimation {
                            duration: Theme.mediumDuration
                            easing.type: Theme.emphasizedEasing
                        }
                    }

                    Behavior on border.color {
                        ColorAnimation {
                            duration: Theme.mediumDuration
                            easing.type: Theme.emphasizedEasing
                        }
                    }

                    Rectangle {
                        id: phaseFlash
                        anchors.fill: parent
                        radius: parent.radius
                        opacity: 0
                        NumberAnimation on opacity {
                            id: phaseFlashAnim
                            running: false
                            from: 0.35; to: 0
                            duration: 800
                            easing.type: Easing.OutCubic
                        }
                    }

                    Loader {
                        id: appIconsLoader
                        anchors.fill: parent
                        active: SettingsData.showWorkspaceApps || SettingsData.showWorkspaceIndex || SettingsData.showWorkspaceName || loadedHasIcon
                        sourceComponent: Item {
                            id: contentRoot
                            readonly property real contentWidth: contentRow.item?.implicitWidth ?? 0
                            readonly property real contentHeight: contentRow.item?.implicitHeight ?? 0

                            Loader {
                                id: contentRow
                                anchors.centerIn: parent
                                sourceComponent: root.isVertical ? columnLayout : rowLayout
                            }

                            Component {
                                id: rowLayout
                                Row {
                                    spacing: 4
                                    visible: loadedIcons.length > 0 || SettingsData.showWorkspaceIndex || SettingsData.showWorkspaceName || loadedHasIcon

                                    // Agent status dot (before name)
                                    Item {
                                        visible: agentDotRect.wsPhase !== "" && agentDotRect.wsPhase !== "idle"
                                        width: 6 + 2
                                        height: root.appIconSize
                                    Rectangle {
                                        id: agentDotRect
                                        readonly property string wsPhase: root.agentPhaseForWs(modelData?.name)
                                        width: 6; height: 6; radius: 3
                                        anchors.centerIn: parent
                                        color: root.agentDotColor(agentDotRect.wsPhase)
                                        SequentialAnimation on opacity {
                                            running: agentDotRect.wsPhase === "waiting_permission" || agentDotRect.wsPhase === "waiting_input"
                                            loops: Animation.Infinite
                                            NumberAnimation { to: 0.3; duration: 800; easing.type: Easing.InOutSine }
                                            NumberAnimation { to: 1.0; duration: 800; easing.type: Easing.InOutSine }
                                        }
                                    }
                                    }

                                    Item {
                                        visible: loadedHasIcon && loadedIconData?.type === "icon"
                                        width: wsIcon.width
                                        height: root.appIconSize

                                        DankIcon {
                                            id: wsIcon
                                            anchors.verticalCenter: parent.verticalCenter
                                            name: loadedIconData?.value ?? ""
                                            size: Theme.barTextSize(barThickness, barConfig?.fontScale, barConfig?.maximizeWidgetText)
                                            color: (isActive || isUrgent) ? Qt.rgba(Theme.surfaceContainer.r, Theme.surfaceContainer.g, Theme.surfaceContainer.b, 0.95) : isPlaceholder ? Theme.surfaceTextAlpha : Theme.surfaceTextMedium
                                            weight: (isActive && !isPlaceholder) ? 500 : 400
                                        }
                                    }

                                    Item {
                                        visible: loadedHasIcon && loadedIconData?.type === "text"
                                        width: wsText.implicitWidth
                                        height: root.appIconSize

                                        StyledText {
                                            id: wsText
                                            anchors.verticalCenter: parent.verticalCenter
                                            text: loadedIconData?.value ?? ""
                                            color: (isActive || isUrgent) ? Qt.rgba(Theme.surfaceContainer.r, Theme.surfaceContainer.g, Theme.surfaceContainer.b, 0.95) : isPlaceholder ? Theme.surfaceTextAlpha : Theme.surfaceTextMedium
                                            font.pixelSize: Theme.barTextSize(barThickness, barConfig?.fontScale, barConfig?.maximizeWidgetText)
                                            font.weight: (isActive && !isPlaceholder) ? Font.DemiBold : Font.Normal
                                        }
                                    }

                                    Item {
                                        visible: ((SettingsData.showWorkspaceIndex || SettingsData.showWorkspaceName) && !loadedHasIcon) || (loadedHasIcon && SettingsData.showWorkspaceName && hasWorkspaceName)
                                        width: wsIndexText.implicitWidth
                                        height: root.appIconSize

                                        StyledText {
                                            id: wsIndexText
                                            anchors.verticalCenter: parent.verticalCenter
                                            text: loadedHasIcon ? (modelData?.name ?? "") : root.getWorkspaceIndex(modelData, index)
                                            color: (isActive || isUrgent) ? Qt.rgba(Theme.surfaceContainer.r, Theme.surfaceContainer.g, Theme.surfaceContainer.b, 0.95) : isPlaceholder ? Theme.surfaceTextAlpha : Theme.surfaceTextMedium
                                            font.pixelSize: Theme.barTextSize(barThickness, barConfig?.fontScale, barConfig?.maximizeWidgetText)
                                            font.weight: (isActive && !isPlaceholder) ? Font.DemiBold : Font.Normal
                                        }
                                    }

                                    Repeater {
                                        model: ScriptModel {
                                            values: loadedIcons.slice(0, SettingsData.maxWorkspaceIcons)
                                        }
                                        delegate: Item {
                                            width: root.appIconSize
                                            height: root.appIconSize
                                            readonly property bool appHighlightActive: SettingsData.workspaceActiveAppHighlightEnabled && modelData.active
                                            readonly property color appBorderColor: appHighlightActive ? focusedBorderColor : Theme.primarySelected
                                            readonly property color appGlyphColor: appHighlightActive ? focusedBorderColor : Theme.primary
                                            readonly property real appOpacity: modelData.active ? 1.0 : rowAppMouseArea.containsMouse ? 0.8 : 0.6

                                            IconImage {
                                                id: rowAppIcon
                                                anchors.fill: parent
                                                source: modelData.icon || ""
                                                opacity: modelData.active ? 1.0 : rowAppMouseArea.containsMouse ? 0.8 : 0.6
                                                visible: !modelData.isQuickshell && !modelData.isSteamApp && status === Image.Ready
                                            }

                                            Rectangle {
                                                anchors.fill: parent
                                                visible: !modelData.isQuickshell && !modelData.isSteamApp && rowAppIcon.status !== Image.Ready
                                                color: Theme.surfaceContainer
                                                radius: Theme.cornerRadius * (root.appIconSize / 40)
                                                border.width: 1
                                                border.color: appBorderColor
                                                opacity: appOpacity

                                                StyledText {
                                                    anchors.centerIn: parent
                                                    text: (modelData.fallbackText || "?").charAt(0).toUpperCase()
                                                    font.pixelSize: parent.width * 0.45
                                                    color: appGlyphColor
                                                    font.weight: Font.Bold
                                                }
                                            }

                                            Rectangle {
                                                anchors.fill: parent
                                                visible: !modelData.isQuickshell && modelData.isSteamApp && rowSteamIcon.status !== Image.Ready
                                                color: Theme.surfaceContainer
                                                radius: Theme.cornerRadius * (root.appIconSize / 40)
                                                border.width: 1
                                                border.color: appBorderColor
                                                opacity: appOpacity

                                                DankIcon {
                                                    anchors.centerIn: parent
                                                    size: parent.width * 0.7
                                                    name: "sports_esports"
                                                    color: appGlyphColor
                                                }
                                            }

                                            IconImage {
                                                anchors.fill: parent
                                                source: modelData.icon
                                                opacity: modelData.active ? 1.0 : rowAppMouseArea.containsMouse ? 0.8 : 0.6
                                                visible: modelData.isQuickshell
                                                layer.enabled: true
                                                layer.effect: MultiEffect {
                                                    saturation: 0
                                                    colorization: 1
                                                    colorizationColor: appHighlightActive ? focusedBorderColor : (isActive ? quickshellIconActiveColor : quickshellIconInactiveColor)
                                                }
                                            }

                                            IconImage {
                                                id: rowSteamIcon
                                                anchors.fill: parent
                                                source: modelData.icon
                                                opacity: modelData.active ? 1.0 : rowAppMouseArea.containsMouse ? 0.8 : 0.6
                                                visible: modelData.isSteamApp && modelData.icon
                                            }

                                            DankIcon {
                                                anchors.centerIn: parent
                                                size: root.appIconSize
                                                name: "sports_esports"
                                                color: appHighlightActive ? focusedBorderColor : Theme.widgetTextColor
                                                opacity: modelData.active ? 1.0 : rowAppMouseArea.containsMouse ? 0.8 : 0.6
                                                visible: modelData.isSteamApp && !modelData.icon
                                            }

                                            Rectangle {
                                                anchors.fill: parent
                                                visible: (rowAppIcon.visible || rowSteamIcon.visible || modelData.isQuickshell) && appHighlightActive
                                                color: "transparent"
                                                radius: Theme.cornerRadius * (root.appIconSize / 40)
                                                border.width: 1
                                                border.color: focusedBorderColor
                                                z: 1
                                            }

                                            MouseArea {
                                                id: rowAppMouseArea
                                                anchors.fill: parent
                                                enabled: isActive
                                                cursorShape: Qt.PointingHandCursor
                                                onClicked: {
                                                    const winId = modelData.windowId;
                                                    if (!winId)
                                                        return;
                                                    NiriService.focusWindow(winId);
                                                }
                                            }

                                            Rectangle {
                                                visible: modelData.count > 1 && !isActive
                                                width: root.appIconSize * 0.67
                                                height: root.appIconSize * 0.67
                                                radius: root.appIconSize * 0.33
                                                color: "black"
                                                border.color: "white"
                                                border.width: 1
                                                anchors.right: parent.right
                                                anchors.bottom: parent.bottom
                                                z: 2

                                                Text {
                                                    anchors.centerIn: parent
                                                    text: modelData.count
                                                    font.pixelSize: root.appIconSize * 0.44
                                                    color: "white"
                                                }
                                            }
                                        }
                                    }

                                }
                            }

                            Component {
                                id: columnLayout
                                Column {
                                    spacing: 4
                                    visible: loadedIcons.length > 0 || SettingsData.showWorkspaceIndex || SettingsData.showWorkspaceName || loadedHasIcon

                                    DankIcon {
                                        visible: loadedHasIcon && loadedIconData?.type === "icon"
                                        anchors.horizontalCenter: parent.horizontalCenter
                                        name: loadedIconData?.value ?? ""
                                        size: Theme.barTextSize(barThickness, barConfig?.fontScale, barConfig?.maximizeWidgetText)
                                        color: (isActive || isUrgent) ? Qt.rgba(Theme.surfaceContainer.r, Theme.surfaceContainer.g, Theme.surfaceContainer.b, 0.95) : isPlaceholder ? Theme.surfaceTextAlpha : Theme.surfaceTextMedium
                                        weight: (isActive && !isPlaceholder) ? 500 : 400
                                    }

                                    StyledText {
                                        visible: loadedHasIcon && loadedIconData?.type === "text"
                                        anchors.horizontalCenter: parent.horizontalCenter
                                        text: loadedIconData?.value ?? ""
                                        color: (isActive || isUrgent) ? Qt.rgba(Theme.surfaceContainer.r, Theme.surfaceContainer.g, Theme.surfaceContainer.b, 0.95) : isPlaceholder ? Theme.surfaceTextAlpha : Theme.surfaceTextMedium
                                        font.pixelSize: Theme.barTextSize(barThickness, barConfig?.fontScale, barConfig?.maximizeWidgetText)
                                        font.weight: (isActive && !isPlaceholder) ? Font.DemiBold : Font.Normal
                                    }

                                    StyledText {
                                        visible: ((SettingsData.showWorkspaceIndex || SettingsData.showWorkspaceName) && !loadedHasIcon) || (loadedHasIcon && SettingsData.showWorkspaceName && hasWorkspaceName)
                                        anchors.horizontalCenter: parent.horizontalCenter
                                        text: loadedHasIcon ? (root.isVertical ? (modelData?.name ?? "").charAt(0) : (modelData?.name ?? "")) : root.getWorkspaceIndex(modelData, index)
                                        color: (isActive || isUrgent) ? Qt.rgba(Theme.surfaceContainer.r, Theme.surfaceContainer.g, Theme.surfaceContainer.b, 0.95) : isPlaceholder ? Theme.surfaceTextAlpha : Theme.surfaceTextMedium
                                        font.pixelSize: Theme.barTextSize(barThickness, barConfig?.fontScale, barConfig?.maximizeWidgetText)
                                        font.weight: (isActive && !isPlaceholder) ? Font.DemiBold : Font.Normal
                                    }

                                    Repeater {
                                        model: ScriptModel {
                                            values: loadedIcons.slice(0, SettingsData.maxWorkspaceIcons)
                                        }
                                        delegate: Item {
                                            width: root.appIconSize
                                            height: root.appIconSize
                                            readonly property bool appHighlightActive: SettingsData.workspaceActiveAppHighlightEnabled && modelData.active
                                            readonly property color appBorderColor: appHighlightActive ? focusedBorderColor : Theme.primarySelected
                                            readonly property color appGlyphColor: appHighlightActive ? focusedBorderColor : Theme.primary
                                            readonly property real appOpacity: modelData.active ? 1.0 : colAppMouseArea.containsMouse ? 0.8 : 0.6

                                            IconImage {
                                                id: colAppIcon
                                                anchors.fill: parent
                                                source: modelData.icon || ""
                                                opacity: modelData.active ? 1.0 : colAppMouseArea.containsMouse ? 0.8 : 0.6
                                                visible: !modelData.isQuickshell && !modelData.isSteamApp && status === Image.Ready
                                            }

                                            Rectangle {
                                                anchors.fill: parent
                                                visible: !modelData.isQuickshell && !modelData.isSteamApp && colAppIcon.status !== Image.Ready
                                                color: Theme.surfaceContainer
                                                radius: Theme.cornerRadius * (root.appIconSize / 40)
                                                border.width: 1
                                                border.color: appBorderColor
                                                opacity: appOpacity

                                                StyledText {
                                                    anchors.centerIn: parent
                                                    text: (modelData.fallbackText || "?").charAt(0).toUpperCase()
                                                    font.pixelSize: parent.width * 0.45
                                                    color: appGlyphColor
                                                    font.weight: Font.Bold
                                                }
                                            }

                                            Rectangle {
                                                anchors.fill: parent
                                                visible: !modelData.isQuickshell && modelData.isSteamApp && colSteamIcon.status !== Image.Ready
                                                color: Theme.surfaceContainer
                                                radius: Theme.cornerRadius * (root.appIconSize / 40)
                                                border.width: 1
                                                border.color: appBorderColor
                                                opacity: appOpacity

                                                DankIcon {
                                                    anchors.centerIn: parent
                                                    size: parent.width * 0.7
                                                    name: "sports_esports"
                                                    color: appGlyphColor
                                                }
                                            }

                                            IconImage {
                                                anchors.fill: parent
                                                source: modelData.icon
                                                opacity: modelData.active ? 1.0 : colAppMouseArea.containsMouse ? 0.8 : 0.6
                                                visible: modelData.isQuickshell
                                                layer.enabled: true
                                                layer.effect: MultiEffect {
                                                    saturation: 0
                                                    colorization: 1
                                                    colorizationColor: appHighlightActive ? focusedBorderColor : (isActive ? quickshellIconActiveColor : quickshellIconInactiveColor)
                                                }
                                            }

                                            IconImage {
                                                id: colSteamIcon
                                                anchors.fill: parent
                                                source: modelData.icon
                                                opacity: modelData.active ? 1.0 : colAppMouseArea.containsMouse ? 0.8 : 0.6
                                                visible: modelData.isSteamApp && modelData.icon
                                            }

                                            DankIcon {
                                                anchors.centerIn: parent
                                                size: root.appIconSize
                                                name: "sports_esports"
                                                color: appHighlightActive ? focusedBorderColor : Theme.widgetTextColor
                                                opacity: modelData.active ? 1.0 : colAppMouseArea.containsMouse ? 0.8 : 0.6
                                                visible: modelData.isSteamApp && !modelData.icon
                                            }

                                            Rectangle {
                                                anchors.fill: parent
                                                visible: (colAppIcon.visible || colSteamIcon.visible || modelData.isQuickshell) && appHighlightActive
                                                color: "transparent"
                                                radius: Theme.cornerRadius * (root.appIconSize / 40)
                                                border.width: 1
                                                border.color: focusedBorderColor
                                                z: 1
                                            }

                                            MouseArea {
                                                id: colAppMouseArea
                                                anchors.fill: parent
                                                enabled: isActive
                                                cursorShape: Qt.PointingHandCursor
                                                onClicked: {
                                                    const winId = modelData.windowId;
                                                    if (!winId)
                                                        return;
                                                    NiriService.focusWindow(winId);
                                                }
                                            }

                                            Rectangle {
                                                visible: modelData.count > 1 && !isActive
                                                width: root.appIconSize * 0.67
                                                height: root.appIconSize * 0.67
                                                radius: root.appIconSize * 0.33
                                                color: "black"
                                                border.color: "white"
                                                border.width: 1
                                                anchors.right: parent.right
                                                anchors.bottom: parent.bottom
                                                z: 2

                                                Text {
                                                    anchors.centerIn: parent
                                                    text: modelData.count
                                                    font.pixelSize: root.appIconSize * 0.44
                                                    color: "white"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                Component.onCompleted: updateAllData()

                Connections {
                    target: CompositorService
                    function onSortedToplevelsChanged() {
                        delegateRoot.updateAllData();
                    }
                }
                Connections {
                    target: NiriService
                    function onAllWorkspacesChanged() {
                        delegateRoot.updateAllData();
                    }
                    function onWindowUrgentChanged() {
                        delegateRoot.updateAllData();
                    }
                    function onWindowsChanged() {
                        delegateRoot.updateAllData();
                    }
                }
                Connections {
                    target: SettingsData
                    function onShowWorkspaceAppsChanged() {
                        delegateRoot.updateAllData();
                    }
                    function onWorkspaceNameIconsChanged() {
                        delegateRoot.updateAllData();
                    }
                    function onAppIdSubstitutionsChanged() {
                        delegateRoot.updateAllData();
                    }
                }
            }
        }
    }

    Component.onCompleted: {
        _updateBlurRegistration();
    }

    property bool _blurRegistered: false
    readonly property bool _shouldBlur: BlurService.enabled && blurBarWindow && blurBarWindow.registerBlurWidget && !(barConfig?.noBackground ?? false) && root.visible && root.width > 0

    on_ShouldBlurChanged: _updateBlurRegistration()

    function _updateBlurRegistration() {
        if (_shouldBlur && !_blurRegistered) {
            blurBarWindow.registerBlurWidget(visualBackground);
            _blurRegistered = true;
        } else if (!_shouldBlur && _blurRegistered) {
            if (blurBarWindow && blurBarWindow.unregisterBlurWidget)
                blurBarWindow.unregisterBlurWidget(visualBackground);
            _blurRegistered = false;
        }
    }

    Component.onDestruction: {
        if (_blurRegistered && blurBarWindow && blurBarWindow.unregisterBlurWidget)
            blurBarWindow.unregisterBlurWidget(visualBackground);
    }
}
