package org.notryx.m0

import org.junit.Assert.*
import org.junit.Test

class MermaidRendererPolicyTest {
    @Test fun requiresExplicitDebugPreview() {
        assertFalse(MermaidRenderPolicy.previewEnabled(false, false))
        assertFalse(MermaidRenderPolicy.previewEnabled(false, true))
        assertFalse(MermaidRenderPolicy.previewEnabled(true, false))
        assertTrue(MermaidRenderPolicy.previewEnabled(true, true))
    }

    @Test fun sourceLimitUsesUtf8BytesAndThemeIsFixed() {
        assertNull(MermaidRenderPolicy.requestError("a".repeat(65536), "light"))
        assertNull(MermaidRenderPolicy.requestError("가".repeat(21845), "dark"))
        assertEquals("mermaid_invalid_request", MermaidRenderPolicy.requestError("a".repeat(65537), "light"))
        assertEquals("mermaid_invalid_request", MermaidRenderPolicy.requestError("가".repeat(21846), "dark"))
        assertEquals("mermaid_invalid_request", MermaidRenderPolicy.requestError("graph TD", "custom"))
    }

    @Test fun allowsOnlyExactBundledScriptGet() {
        val script = "https://notryx-mermaid.invalid/renderer.js"
        assertTrue(MermaidRenderPolicy.allowBundleRequest(script, "GET"))
        for (url in listOf(
            "$script?query=1", "$script#fragment", "https://notryx-mermaid.invalid/other.js",
            "https://example.invalid/renderer.js", "file:///renderer.js", "content://renderer.js",
            "https://notryx-mermaid.invalid:443/renderer.js",
        )) {
            assertFalse(MermaidRenderPolicy.allowBundleRequest(url, "GET"))
        }
        assertFalse(MermaidRenderPolicy.allowBundleRequest(script, "POST"))
    }
}
