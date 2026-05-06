import Quickshell
import Quickshell.Io
import QtQuick

ShellRoot {
    AgoraPicker {
        id: picker
        visible: false
        Component.onCompleted: showTimer.start()
        Timer {
            id: showTimer
            interval: 80
            onTriggered: picker.visible = true
        }
    }

    IpcHandler {
        target: "picker"
        function toggle() { picker.toggleMode("agents"); return "ok" }
        function agents() { picker.toggleMode("agents"); return "ok" }
        function projects() { picker.toggleMode("projects"); return "ok" }
    }
}
