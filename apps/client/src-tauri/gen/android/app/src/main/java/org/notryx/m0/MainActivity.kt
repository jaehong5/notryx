package org.notryx.m0

import android.os.Bundle
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    if (BuildConfig.DEBUG) {
      Thread {
        val result = runCatching { CredentialProbe.run() }.getOrDefault(false)
        android.util.Log.i("NotryxM0", "credential_probe_passed=$result")
      }.start()
    }
  }
}
