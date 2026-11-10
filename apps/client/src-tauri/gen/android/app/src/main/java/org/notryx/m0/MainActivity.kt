package org.notryx.m0

import android.os.Bundle
import android.view.View
import android.webkit.WebView
import android.widget.FrameLayout
import androidx.activity.OnBackPressedCallback
import androidx.activity.enableEdgeToEdge
import androidx.core.graphics.Insets
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat

class MainActivity : TauriActivity() {
  private var mainWebView: WebView? = null

  override fun setContentView(view: View?) {
    if (view !is WebView) {
      super.setContentView(view)
      return
    }
    val content = FrameLayout(this)
    content.addView(view, FrameLayout.LayoutParams(
      FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT,
    ))
    ViewCompat.setOnApplyWindowInsetsListener(content) { container, windowInsets ->
      val handled = WindowInsetsCompat.Type.systemBars() or
        WindowInsetsCompat.Type.displayCutout() or WindowInsetsCompat.Type.ime()
      val inset = windowInsets.getInsets(handled)
      container.setPadding(inset.left, inset.top, inset.right, inset.bottom)
      // Resize the WebView itself; zero handled insets so CSS does not add them again.
      // Propagate updates instead of consuming them, including when the keyboard hides.
      WindowInsetsCompat.Builder(windowInsets).setInsets(handled, Insets.NONE).build()
    }
    super.setContentView(content)
    ViewCompat.requestApplyInsets(content)
  }

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    mainWebView = webView
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
      override fun handleOnBackPressed() {
        val webView = mainWebView ?: return
        if (ViewCompat.getRootWindowInsets(webView)?.isVisible(WindowInsetsCompat.Type.ime()) == true) {
          WindowInsetsControllerCompat(window, webView).hide(WindowInsetsCompat.Type.ime())
          return
        }
        // The renderer saves before requesting background. Missing JS must never bypass that save.
        webView.evaluateJavascript(
          "window.dispatchEvent(new Event('android-back-requested', {cancelable:true}));",
          null,
        )
      }
    })
    if (BuildConfig.DEBUG) {
      Thread {
        val result = runCatching { CredentialProbe.run() }.getOrDefault(false)
        android.util.Log.i("NotryxM0", "credential_probe_passed=$result")
      }.start()
    }
  }
}
