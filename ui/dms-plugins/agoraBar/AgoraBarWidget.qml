import QtQuick
import Quickshell
import Quickshell.Io
import qs.Common
import qs.Services
import qs.Widgets
import qs.Modules.Plugins

PluginComponent {
    id: root

    property var agents: []
    property int runningCount: 0
    property int waitingCount: 0
    property int idleCount: 0

    property int permissionCount: 0

    property var needsYou: []
    property var running: []
    property var idle: []

    function recompute() {
        let r = 0, w = 0, i = 0, p = 0
        const ny = [], rn = [], id = []
        for (const a of agents) {
            if (a.phase === "waiting_permission") { p++; ny.push(a) }
            else if (a.phase === "waiting_input") { w++; ny.push(a) }
            else if (a.phase === "running") { r++; rn.push(a) }
            else if (a.phase === "idle") { i++; id.push(a) }
        }
        runningCount = r
        waitingCount = w
        idleCount = i
        permissionCount = p
        needsYou = ny
        running = rn
        idle = id
    }

    function reload() {
        if (!agentsProcess.running) agentsProcess.running = true
    }

    onPluginServiceChanged: {
        if (pluginService) reload()
    }

    Process {
        id: agentsProcess
        command: ["/home/jay/.local/bin/agora", "agents", "--json"]
        running: false
        stdout: StdioCollector {
            onStreamFinished: {
                try {
                    root.agents = JSON.parse(text.trim() || "[]")
                    root.recompute()
                } catch (e) {
                    root.agents = []
                    root.recompute()
                }
            }
        }
    }

    Timer {
        interval: 3000
        repeat: true
        running: true
        onTriggered: root.reload()
    }

    function pillColor() {
        if (waitingCount > 0) return "#F44336"
        if (runningCount > 0) return "#4CAF50"
        return Theme.surfaceVariantText
    }

    function pillIcon() {
        if (waitingCount > 0) return "priority_high"
        if (runningCount > 0) return "play_arrow"
        return "smart_toy"
    }

    function pillText() {
        const total = runningCount + waitingCount + idleCount
        if (total === 0) return ""
        if (waitingCount > 0) return waitingCount + "!"
        return String(total)
    }

    // The "star" agent: highest-priority one we feature in the pill.
    property var starAgent: agents.length > 0 ? agents[0] : null

    function starColor() {
        if (!starAgent) return Theme.surfaceVariantText
        if (starAgent.phase === "waiting_permission") return "#FFA726"
        if (starAgent.phase === "waiting_input") return "#EF5350"
        if (starAgent.phase === "running") return "#66BB6A"
        return Theme.surfaceVariantText
    }

    function starLabel() {
        if (!starAgent) return ""
        return starAgent.project || starAgent.host || "agent"
    }

    horizontalBarPill: Component {
        Row {
            spacing: 6

            // Pulsing status dot
            Rectangle {
                id: statusDot
                visible: root.agents.length > 0
                width: 8; height: 8; radius: 4
                color: root.starColor()
                anchors.verticalCenter: parent.verticalCenter
                SequentialAnimation on opacity {
                    running: root.waitingCount > 0 || root.permissionCount > 0
                    loops: Animation.Infinite
                    NumberAnimation { to: 0.3; duration: 800; easing.type: Easing.InOutSine }
                    NumberAnimation { to: 1.0; duration: 800; easing.type: Easing.InOutSine }
                }
            }

            // Star agent description
            StyledText {
                visible: root.agents.length > 0
                text: root.starLabel()
                font.pixelSize: Theme.fontSizeSmall
                font.weight: Font.Medium
                color: Theme.surfaceText
                anchors.verticalCenter: parent.verticalCenter
            }

            // Count badge (when > 1 agent)
            Rectangle {
                visible: root.agents.length > 1
                width: countText.implicitWidth + 8
                height: 16
                radius: 8
                color: Qt.rgba(Theme.surfaceVariantText.r, Theme.surfaceVariantText.g,
                               Theme.surfaceVariantText.b, 0.25)
                anchors.verticalCenter: parent.verticalCenter
                StyledText {
                    id: countText
                    anchors.centerIn: parent
                    text: String(root.agents.length)
                    font.pixelSize: Theme.fontSizeSmall - 1
                    font.weight: Font.Bold
                    color: Theme.surfaceText
                }
            }

            // Zero agents
            DankIcon {
                visible: root.agents.length === 0
                name: "smart_toy"
                size: Theme.iconSize - 8
                color: Theme.surfaceVariantText
                anchors.verticalCenter: parent.verticalCenter
            }
        }
    }

    verticalBarPill: Component {
        Column {
            spacing: 2
            Rectangle {
                width: 8; height: 8; radius: 4
                color: root.starColor()
                visible: root.agents.length > 0
                anchors.horizontalCenter: parent.horizontalCenter
            }
            StyledText {
                text: String(root.agents.length)
                visible: root.agents.length > 0
                font.pixelSize: Theme.fontSizeSmall - 2
                font.weight: Font.Medium
                color: root.pillColor()
                anchors.horizontalCenter: parent.horizontalCenter
            }
        }
    }

    popoutContent: Component {
        PopoutComponent {
            id: popout

            headerText: "Agents"
            detailsText: root.agents.length === 0 ? "No active sessions" : ""
            showCloseButton: true

            Column {
                width: parent.width
                spacing: Theme.spacingXS

                Row {
                    visible: root.agents.length > 0
                    spacing: 4
                    leftPadding: 2
                    StyledText {
                        visible: root.permissionCount > 0
                        text: root.permissionCount + " permission"
                        font.pixelSize: Theme.fontSizeSmall
                        color: "#FFA726"
                    }
                    StyledText {
                        visible: root.permissionCount > 0 && root.waitingCount > 0
                        text: " · "
                        font.pixelSize: Theme.fontSizeSmall
                        color: Theme.surfaceVariantText
                    }
                    StyledText {
                        visible: root.waitingCount > 0
                        text: root.waitingCount + " waiting"
                        font.pixelSize: Theme.fontSizeSmall
                        color: "#EF5350"
                    }
                    StyledText {
                        visible: (root.permissionCount > 0 || root.waitingCount > 0) && root.runningCount > 0
                        text: " · "
                        font.pixelSize: Theme.fontSizeSmall
                        color: Theme.surfaceVariantText
                    }
                    StyledText {
                        visible: root.runningCount > 0
                        text: root.runningCount + " running"
                        font.pixelSize: Theme.fontSizeSmall
                        color: "#66BB6A"
                    }
                    StyledText {
                        visible: (root.permissionCount > 0 || root.waitingCount > 0 || root.runningCount > 0) && root.idleCount > 0
                        text: " · "
                        font.pixelSize: Theme.fontSizeSmall
                        color: Theme.surfaceVariantText
                    }
                    StyledText {
                        visible: root.idleCount > 0
                        text: root.idleCount + " idle"
                        font.pixelSize: Theme.fontSizeSmall
                        color: Theme.surfaceVariantText
                    }
                }

                Row {
                    visible: root.needsYou.length > 0
                    spacing: 4
                    leftPadding: 2
                    topPadding: Theme.spacingS
                    DankIcon {
                        name: "priority_high"
                        size: Theme.fontSizeSmall
                        color: "#EF5350"
                        anchors.verticalCenter: parent.verticalCenter
                    }
                    StyledText {
                        text: "NEEDS YOU"
                        font.pixelSize: Theme.fontSizeSmall
                        font.weight: Font.Bold
                        color: "#EF5350"
                        anchors.verticalCenter: parent.verticalCenter
                    }
                }
                Repeater {
                    model: root.needsYou
                    Rectangle {
                        id: nyCard
                        width: parent.width
                        height: nyCol.implicitHeight + Theme.spacingS * 2
                        radius: 0
                        color: nyMa.containsMouse
                            ? Qt.lighter(Theme.surfaceContainer, 1.15)
                            : Theme.surfaceContainer
                        Behavior on color { ColorAnimation { duration: 150 } }
                        clip: true
                        opacity: 0
                        Component.onCompleted: opacity = 1
                        Behavior on opacity { NumberAnimation { duration: 250; easing.type: Easing.OutCubic } }
                        Rectangle {
                            id: nyAccent
                            width: 3; height: parent.height
                            color: modelData.phase === "waiting_permission" ? "#FFA726" : "#EF5350"
                            SequentialAnimation on opacity {
                                loops: Animation.Infinite
                                NumberAnimation { to: 0.4; duration: 1200; easing.type: Easing.InOutSine }
                                NumberAnimation { to: 1.0; duration: 1200; easing.type: Easing.InOutSine }
                            }
                        }
                        MouseArea {
                            id: nyMa
                            anchors.fill: parent
                            cursorShape: Qt.PointingHandCursor
                            hoverEnabled: true
                            onClicked: {
                                Quickshell.execDetached(["/home/jay/.local/bin/agora", "focus-agent", modelData.session_id])
                                popout.close()
                            }
                        }
                        Column {
                            id: nyCol
                            anchors.fill: parent
                            anchors.margins: Theme.spacingS
                            anchors.leftMargin: Theme.spacingS + 3
                            spacing: 4
                            Item {
                                width: parent.width
                                height: nyProj.height
                                StyledText {
                                    id: nyProj
                                    text: modelData.project || "(no project)"
                                    font.weight: Font.Bold
                                    color: Theme.surfaceText
                                    anchors.verticalCenter: parent.verticalCenter
                                }
                                Rectangle {
                                    anchors.right: parent.right
                                    anchors.verticalCenter: parent.verticalCenter
                                    width: nyBadgeText.implicitWidth + 10
                                    height: nyBadgeText.implicitHeight + 4
                                    radius: height / 2
                                    color: Qt.rgba(Theme.surfaceVariantText.r, Theme.surfaceVariantText.g, Theme.surfaceVariantText.b, 0.15)
                                    StyledText {
                                        id: nyBadgeText
                                        anchors.centerIn: parent
                                        text: (modelData.host || "local") + " · " + modelData.session_id.substring(0, 8)
                                        font.pixelSize: Theme.fontSizeSmall - 2
                                        color: Theme.surfaceVariantText
                                    }
                                }
                            }
                            StyledText {
                                visible: !!modelData.last_prompt
                                text: modelData.last_prompt || ""
                                font.pixelSize: Theme.fontSizeSmall - 1
                                color: Theme.surfaceVariantText
                                opacity: 0.8
                                wrapMode: Text.WordWrap
                                width: parent.width
                                maximumLineCount: 1
                                elide: Text.ElideRight
                            }
                        }
                    }
                }

                Row {
                    visible: root.running.length > 0
                    spacing: 4
                    leftPadding: 2
                    topPadding: Theme.spacingS
                    DankIcon {
                        name: "play_arrow"
                        size: Theme.fontSizeSmall
                        color: "#66BB6A"
                        anchors.verticalCenter: parent.verticalCenter
                    }
                    StyledText {
                        text: "RUNNING"
                        font.pixelSize: Theme.fontSizeSmall
                        font.weight: Font.Bold
                        color: "#66BB6A"
                        anchors.verticalCenter: parent.verticalCenter
                    }
                }
                Repeater {
                    model: root.running
                    Rectangle {
                        width: parent.width
                        height: rnCol.implicitHeight + Theme.spacingS * 2
                        radius: 0
                        color: rnMa.containsMouse
                            ? Qt.lighter(Theme.surfaceContainer, 1.15)
                            : Theme.surfaceContainer
                        Behavior on color { ColorAnimation { duration: 150 } }
                        clip: true
                        opacity: 0
                        Component.onCompleted: opacity = 1
                        Behavior on opacity { NumberAnimation { duration: 250; easing.type: Easing.OutCubic } }
                        Rectangle {
                            width: 3; height: parent.height
                            color: "#66BB6A"
                        }
                        MouseArea {
                            id: rnMa
                            anchors.fill: parent
                            cursorShape: Qt.PointingHandCursor
                            hoverEnabled: true
                            onClicked: {
                                Quickshell.execDetached(["/home/jay/.local/bin/agora", "focus-agent", modelData.session_id])
                                popout.close()
                            }
                        }
                        Column {
                            id: rnCol
                            anchors.fill: parent
                            anchors.margins: Theme.spacingS
                            anchors.leftMargin: Theme.spacingS + 3
                            spacing: 4
                            Item {
                                width: parent.width
                                height: rnProj.height
                                StyledText {
                                    id: rnProj
                                    text: modelData.project || "(no project)"
                                    font.weight: Font.Bold
                                    color: Theme.surfaceText
                                    anchors.verticalCenter: parent.verticalCenter
                                }
                                Rectangle {
                                    anchors.right: parent.right
                                    anchors.verticalCenter: parent.verticalCenter
                                    width: rnBadgeText.implicitWidth + 10
                                    height: rnBadgeText.implicitHeight + 4
                                    radius: height / 2
                                    color: Qt.rgba(Theme.surfaceVariantText.r, Theme.surfaceVariantText.g, Theme.surfaceVariantText.b, 0.15)
                                    StyledText {
                                        id: rnBadgeText
                                        anchors.centerIn: parent
                                        text: (modelData.host || "local") + " · " + modelData.session_id.substring(0, 8)
                                        font.pixelSize: Theme.fontSizeSmall - 2
                                        color: Theme.surfaceVariantText
                                    }
                                }
                            }
                            StyledText {
                                visible: !!modelData.last_prompt
                                text: modelData.last_prompt || ""
                                font.pixelSize: Theme.fontSizeSmall - 1
                                color: Theme.surfaceVariantText
                                opacity: 0.8
                                wrapMode: Text.WordWrap
                                width: parent.width
                                maximumLineCount: 1
                                elide: Text.ElideRight
                            }
                        }
                    }
                }

                Row {
                    visible: root.idle.length > 0
                    spacing: 4
                    leftPadding: 2
                    topPadding: Theme.spacingS
                    DankIcon {
                        name: "pause"
                        size: Theme.fontSizeSmall
                        color: Theme.surfaceVariantText
                        anchors.verticalCenter: parent.verticalCenter
                    }
                    StyledText {
                        text: "IDLE"
                        font.pixelSize: Theme.fontSizeSmall
                        font.weight: Font.Bold
                        color: Theme.surfaceVariantText
                        anchors.verticalCenter: parent.verticalCenter
                    }
                }
                Repeater {
                    model: root.idle
                    Rectangle {
                        width: parent.width
                        height: idCol.implicitHeight + Theme.spacingS * 2
                        radius: 0
                        color: idMa.containsMouse
                            ? Qt.lighter(Theme.surfaceContainer, 1.15)
                            : Theme.surfaceContainer
                        Behavior on color { ColorAnimation { duration: 150 } }
                        clip: true
                        opacity: 0
                        Component.onCompleted: opacity = 1
                        Behavior on opacity { NumberAnimation { duration: 250; easing.type: Easing.OutCubic } }
                        Rectangle {
                            width: 3; height: parent.height
                            color: Theme.surfaceVariantText
                        }
                        MouseArea {
                            id: idMa
                            anchors.fill: parent
                            cursorShape: Qt.PointingHandCursor
                            hoverEnabled: true
                            onClicked: {
                                Quickshell.execDetached(["/home/jay/.local/bin/agora", "focus-agent", modelData.session_id])
                                popout.close()
                            }
                        }
                        Column {
                            id: idCol
                            anchors.fill: parent
                            anchors.margins: Theme.spacingS
                            anchors.leftMargin: Theme.spacingS + 3
                            spacing: 4
                            Item {
                                width: parent.width
                                height: idProj.height
                                StyledText {
                                    id: idProj
                                    text: modelData.project || "(no project)"
                                    font.weight: Font.Bold
                                    color: Theme.surfaceText
                                    anchors.verticalCenter: parent.verticalCenter
                                }
                                Rectangle {
                                    anchors.right: parent.right
                                    anchors.verticalCenter: parent.verticalCenter
                                    width: idBadgeText.implicitWidth + 10
                                    height: idBadgeText.implicitHeight + 4
                                    radius: height / 2
                                    color: Qt.rgba(Theme.surfaceVariantText.r, Theme.surfaceVariantText.g, Theme.surfaceVariantText.b, 0.15)
                                    StyledText {
                                        id: idBadgeText
                                        anchors.centerIn: parent
                                        text: (modelData.host || "local") + " · " + modelData.session_id.substring(0, 8)
                                        font.pixelSize: Theme.fontSizeSmall - 2
                                        color: Theme.surfaceVariantText
                                    }
                                }
                            }
                            StyledText {
                                visible: !!modelData.last_prompt
                                text: modelData.last_prompt || ""
                                font.pixelSize: Theme.fontSizeSmall - 1
                                color: Theme.surfaceVariantText
                                opacity: 0.8
                                wrapMode: Text.WordWrap
                                width: parent.width
                                maximumLineCount: 1
                                elide: Text.ElideRight
                            }
                        }
                    }
                }
            }
        }
    }
}
