import Quickshell
import Quickshell.Io
import QtQuick

ShellRoot {
    AgoraPicker {
        id: picker
        visible: false
        Component.onCompleted: {
            var env = Quickshell.env("AGORA_PICKER_MODE")
            if (env) picker.mode = env
            showTimer.start()
        }
        Timer {
            id: showTimer
            interval: 80
            onTriggered: picker.toggleMode(picker.mode)
        }
    }

    IpcHandler {
        target: "picker"
        function toggle() { picker.toggleMode("agents"); return "ok" }
        function agents() { picker.toggleMode("agents"); return "ok" }
        function projects() { picker.toggleMode("projects"); return "ok" }
    }
}
