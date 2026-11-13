package org.notryx.m0

import android.annotation.SuppressLint
import android.content.Context
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.os.Message
import android.os.SystemClock
import android.view.View
import android.webkit.ConsoleMessage
import android.webkit.CookieManager
import android.webkit.GeolocationPermissions
import android.webkit.JsPromptResult
import android.webkit.JsResult
import android.webkit.PermissionRequest
import android.webkit.RenderProcessGoneDetail
import android.webkit.ValueCallback
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import org.json.JSONObject
import java.io.ByteArrayInputStream

internal object MermaidRenderPolicy {
    const val BUNDLE_URL = "https://notryx-mermaid.invalid/renderer.js"
    fun previewEnabled(debug: Boolean, requested: Boolean) = debug && requested
    fun requestError(source: String, theme: String): String? =
        if (source.toByteArray(Charsets.UTF_8).size > 65536 || theme !in setOf("light", "dark"))
            "mermaid_invalid_request" else null
    fun allowBundleRequest(url: String, method: String) = url == BUNDLE_URL && method == "GET"
}

internal class MermaidRenderer(private val context: Context) {
    private class Render(var surface: WebView? = null, var result: String? = null, var started: Boolean = false)
    private val handler = Handler(Looper.getMainLooper())
    private val renders = mutableMapOf<String, Render>()

    @SuppressLint("SetJavaScriptEnabled")
    @Suppress("DEPRECATION")
    fun start(id: String, request: String): String {
        if (id.isEmpty() || id.length > 128) return "mermaid_invalid_request"
        if (renders.size >= 2 || renders.containsKey(id)) return "mermaid_busy"
        val input = try {
            val parsed = JSONObject(request)
            val source = parsed.opt("source") as? String ?: return "mermaid_invalid_request"
            val theme = parsed.opt("theme") as? String ?: return "mermaid_invalid_request"
            MermaidRenderPolicy.requestError(source, theme)?.let { return it }
            JSONObject().put("source", source).put("theme", theme).toString()
        } catch (_: Exception) { return "mermaid_invalid_request" }
        val render = Render()
        renders[id] = render
        try {
            val surface = WebView(context)
            render.surface = surface
            surface.settings.apply {
                javaScriptEnabled = true
                allowFileAccess = false
                allowContentAccess = false
                allowFileAccessFromFileURLs = false
                allowUniversalAccessFromFileURLs = false
                domStorageEnabled = false
                databaseEnabled = false
                blockNetworkLoads = true
                blockNetworkImage = true
                loadsImagesAutomatically = false
                mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
                javaScriptCanOpenWindowsAutomatically = false
                setSupportMultipleWindows(false)
                setGeolocationEnabled(false)
                cacheMode = WebSettings.LOAD_NO_CACHE
            }
            // CookieManager's global policy belongs to the main application WebView.
            CookieManager.getInstance().setAcceptThirdPartyCookies(surface, false)
            surface.setDownloadListener { _, _, _, _, _ -> }
            surface.webChromeClient = object : WebChromeClient() {
                override fun onConsoleMessage(message: ConsoleMessage) = true
                override fun onJsAlert(view: WebView, url: String, message: String, result: JsResult): Boolean {
                    result.cancel()
                    return true
                }
                override fun onJsConfirm(view: WebView, url: String, message: String, result: JsResult): Boolean {
                    result.cancel()
                    return true
                }
                override fun onJsPrompt(view: WebView, url: String, message: String, defaultValue: String, result: JsPromptResult): Boolean {
                    result.cancel()
                    return true
                }
                override fun onPermissionRequest(request: PermissionRequest) = request.deny()
                override fun onGeolocationPermissionsShowPrompt(origin: String, callback: GeolocationPermissions.Callback) =
                    callback.invoke(origin, false, false)
                override fun onCreateWindow(view: WebView, dialog: Boolean, gesture: Boolean, result: Message) = false
                override fun onShowFileChooser(view: WebView, callback: ValueCallback<Array<Uri>>, params: FileChooserParams): Boolean {
                    callback.onReceiveValue(null)
                    return true
                }
            }
            surface.webViewClient = object : WebViewClient() {
                override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest) = true
                override fun shouldOverrideUrlLoading(view: WebView, url: String) = true
                override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse {
                    if (!request.isForMainFrame && MermaidRenderPolicy.allowBundleRequest(request.url.toString(), request.method)) {
                        return try {
                            WebResourceResponse("application/javascript", "UTF-8", context.assets.open("renderer.js"))
                        } catch (_: Exception) { denied() }
                    }
                    return denied()
                }
                override fun shouldInterceptRequest(view: WebView, url: String) = denied()
                override fun onPageFinished(view: WebView, url: String) {
                    if (!active(id, render) || render.started) return
                    if (url != ORIGIN) {
                        finish(id, render, failure("mermaid_render_failed"))
                        return
                    }
                    render.started = true
                    try {
                        view.evaluateJavascript("window.__NOTRYX_ANDROID_MERMAID_START__($input);null;", null)
                        readResult(id, render)
                    } catch (_: Exception) { finish(id, render, failure("mermaid_render_failed")) }
                }
                override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {
                    finish(id, render, failure("mermaid_render_failed"))
                    return true
                }
            }
            surface.measure(View.MeasureSpec.makeMeasureSpec(1024, View.MeasureSpec.EXACTLY),
                View.MeasureSpec.makeMeasureSpec(768, View.MeasureSpec.EXACTLY))
            surface.layout(0, 0, 1024, 768)
            later(render, 15000) { finish(id, render, failure("mermaid_render_timeout")) }
            surface.loadDataWithBaseURL(ORIGIN, PAGE, "text/html", "UTF-8", null)
        } catch (_: Exception) {
            cancel(id)
            return "mermaid_render_failed"
        }
        return ""
    }

    fun poll(id: String): String {
        val render = renders[id] ?: return failure("mermaid_render_failed")
        val result = render.result ?: return ""
        renders.remove(id)
        return result
    }

    fun cancel(id: String) { renders.remove(id)?.let(::dispose) }

    fun cancelAll() {
        val pending = renders.values.toList()
        renders.clear()
        pending.forEach(::dispose)
    }

    private fun active(id: String, render: Render) = renders[id] === render && render.result == null

    private fun readResult(id: String, render: Render) {
        if (!active(id, render)) return
        try {
            render.surface?.evaluateJavascript("window.__NOTRYX_ANDROID_MERMAID_RESULT__") { value ->
                if (!active(id, render)) return@evaluateJavascript
                if (value == "null") {
                    later(render, 50) { readResult(id, render) }
                    return@evaluateJavascript
                }
                finish(id, render, checkedResult(value))
            }
        } catch (_: Exception) { finish(id, render, failure("mermaid_render_failed")) }
    }

    private fun checkedResult(value: String): String {
        if (value.toByteArray(Charsets.UTF_8).size > SVG_BYTES * 6 + 1024) return failure("mermaid_result_too_large")
        return try {
            val result = JSONObject(value)
            val svg = result.opt("svg") as? String
            val error = result.opt("error") as? String
            when {
                svg != null && svg.toByteArray(Charsets.UTF_8).size > SVG_BYTES -> failure("mermaid_result_too_large")
                !svg.isNullOrEmpty() && result.isNull("error") -> JSONObject().put("svg", svg).put("error", JSONObject.NULL).toString()
                result.isNull("svg") && error in ERRORS -> failure(error!!)
                else -> failure("mermaid_render_failed")
            }
        } catch (_: Exception) { failure("mermaid_render_failed") }
    }

    private fun finish(id: String, render: Render, result: String) {
        if (!active(id, render)) return
        render.result = result
        dispose(render)
    }

    private fun later(render: Render, delay: Long, action: () -> Unit) {
        handler.postAtTime(Runnable { action() }, render, SystemClock.uptimeMillis() + delay)
    }

    private fun dispose(render: Render) {
        handler.removeCallbacksAndMessages(render)
        val surface = render.surface ?: return
        render.surface = null
        runCatching { surface.stopLoading() }
        runCatching { surface.destroy() }
    }

    companion object {
        private const val SVG_BYTES = 2 * 1024 * 1024
        private const val ORIGIN = "https://notryx-mermaid.invalid/"
        private val ERRORS = setOf("mermaid_source_limit", "mermaid_document_config", "mermaid_unsupported_diagram",
            "mermaid_unsupported", "mermaid_edge_limit", "mermaid_parse_error", "mermaid_invalid_request")
        private val PAGE = """<!doctype html><html><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src https://notryx-mermaid.invalid/renderer.js; style-src 'unsafe-inline'; img-src 'none'; connect-src 'none'; frame-src 'none'; worker-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'"></head><body><script src="/renderer.js"></script></body></html>"""
        private fun denied() = WebResourceResponse("text/plain", "UTF-8", 403, "Forbidden", emptyMap(), ByteArrayInputStream(byteArrayOf()))
        private fun failure(code: String) = JSONObject().put("svg", JSONObject.NULL).put("error", code).toString()
    }
}
