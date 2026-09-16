package dev.netsanctum.desktop

import android.content.res.Configuration
import android.os.Bundle
import androidx.activity.OnBackPressedCallback
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
      override fun handleOnBackPressed() {
        if (NodeViewPlugin.handleBackPressed()) return
        isEnabled = false
        onBackPressedDispatcher.onBackPressed()
        isEnabled = true
      }
    })
  }

  override fun onUserLeaveHint() {
    NodeViewPlugin.enterPictureInPicture()
    super.onUserLeaveHint()
  }

  override fun onPictureInPictureModeChanged(
    isInPictureInPictureMode: Boolean,
    newConfig: Configuration,
  ) {
    super.onPictureInPictureModeChanged(isInPictureInPictureMode, newConfig)
    NodeViewPlugin.pictureInPictureModeChanged(isInPictureInPictureMode)
  }

  override fun onDestroy() {
    NodeViewPlugin.activityDestroyed(this)
    super.onDestroy()
  }
}
