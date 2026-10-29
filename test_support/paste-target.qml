import QtQuick
import QtQuick.Controls
import Quickshell
import Quickshell.Io

// Only launched with the acceptance runner's private Wayland socket and HOME.
FloatingWindow {
    id: window
    title: Quickshell.env("CLIP_ACCEPTANCE_TITLE")
    visible: true
    implicitWidth: 480
    implicitHeight: 240

    FileView {
        id: received
        path: Quickshell.env("CLIP_ACCEPTANCE_RECEIVED")
    }

    TextArea {
        anchors.fill: parent
        focus: true
        Component.onCompleted: forceActiveFocus()
        onTextChanged: received.setText(text)
    }
}
