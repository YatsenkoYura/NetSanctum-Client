package dev.netsanctum.desktop

import android.app.Activity
import android.app.Dialog
import android.app.NotificationChannel
import android.app.NotificationManager
import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.net.Uri
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.text.TextUtils
import android.util.Base64
import android.view.Gravity
import android.view.KeyEvent
import android.view.ViewGroup
import android.webkit.CookieManager
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.PopupWindow
import android.widget.ProgressBar
import android.widget.TextView
import android.widget.Toast
import androidx.core.app.ActivityCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.pm.ShortcutInfoCompat
import androidx.core.content.pm.ShortcutManagerCompat
import androidx.core.graphics.drawable.IconCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

@InvokeArg
class OpenNodeArgs {
  lateinit var nodeUrl: String
  lateinit var cookieName: String
  lateinit var cookieValue: String
}

@InvokeArg
class DownloadNotificationArgs {
  lateinit var status: String
  var progress: Double = 0.0
  lateinit var message: String
}

@InvokeArg
class CreateShortcutArgs {
  lateinit var nodeOrigin: String
  lateinit var moduleId: String
  lateinit var name: String
}

@InvokeArg
class VaultKeyArgs {
  lateinit var value: String
}

@TauriPlugin
class NodeViewPlugin(private val activity: Activity) : Plugin(activity) {
  private var shellWebView: WebView? = null
  private var notificationPermissionRequested = false
  private var pendingShortcutNode: String? = null
  private var pendingShortcutModule: String? = null

  override fun load(webView: WebView) {
    shellWebView = webView
    createNotificationChannel()
    captureShortcut(activity.intent)
  }

  override fun onNewIntent(intent: Intent) {
    captureShortcut(intent)
    dispatchPendingShortcut()
  }

  @Command
  fun createShortcut(invoke: Invoke) {
    val args = invoke.parseArgs(CreateShortcutArgs::class.java)
    val nodeUri = Uri.parse(args.nodeOrigin)
    val name = args.name.trim().take(40)
    if (nodeUri.host.isNullOrEmpty() || !validId(args.moduleId) || name.isEmpty()) {
      invoke.reject("Invalid shortcut")
      return
    }
    if (!ShortcutManagerCompat.isRequestPinShortcutSupported(activity)) {
      invoke.reject("Launcher does not support pinned shortcuts")
      return
    }
    val shortcutIntent = Intent(activity, MainActivity::class.java).apply {
      action = ACTION_OPEN_OFFLINE_MODULE
      putExtra(EXTRA_NODE_ORIGIN, args.nodeOrigin)
      putExtra(EXTRA_MODULE_ID, args.moduleId)
      flags = Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP
    }
    val shortcut = ShortcutInfoCompat.Builder(
      activity,
      "${args.nodeOrigin}:${args.moduleId}".hashCode().toUInt().toString(16),
    )
      .setShortLabel(name)
      .setLongLabel("$name · ${nodeUri.host}")
      .setIcon(IconCompat.createWithBitmap(shortcutIcon(args.moduleId)))
      .setIntent(shortcutIntent)
      .build()
    if (ShortcutManagerCompat.requestPinShortcut(activity, shortcut, null)) {
      invoke.resolve()
    } else {
      invoke.reject("Could not request pinned shortcut")
    }
  }

  @Command
  fun takeShortcut(invoke: Invoke) {
    val nodeOrigin = pendingShortcutNode
    val moduleId = pendingShortcutModule
    if (nodeOrigin == null || moduleId == null) {
      invoke.resolve()
      return
    }
    pendingShortcutNode = null
    pendingShortcutModule = null
    invoke.resolve(app.tauri.plugin.JSObject().apply {
      put("nodeOrigin", nodeOrigin)
      put("moduleId", moduleId)
    })
  }

  @Command
  fun storeVaultKey(invoke: Invoke) {
    try {
      val cipher = Cipher.getInstance("AES/GCM/NoPadding")
      cipher.init(Cipher.ENCRYPT_MODE, getOrCreateVaultKey())
      val encrypted = cipher.doFinal(invoke.parseArgs(VaultKeyArgs::class.java).value.toByteArray())
      activity.getSharedPreferences(VAULT_PREFS, Activity.MODE_PRIVATE).edit()
        .putString("iv", Base64.encodeToString(cipher.iv, Base64.NO_WRAP))
        .putString("value", Base64.encodeToString(encrypted, Base64.NO_WRAP))
        .apply()
      invoke.resolve()
    } catch (error: Exception) {
      invoke.reject("Could not protect the vault key: ${error.message}")
    }
  }

  @Command
  fun loadVaultKey(invoke: Invoke) {
    try {
      val preferences = activity.getSharedPreferences(VAULT_PREFS, Activity.MODE_PRIVATE)
      val iv = preferences.getString("iv", null)
      val encrypted = preferences.getString("value", null)
      if (iv == null || encrypted == null) {
        invoke.resolve()
        return
      }
      val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
      val key = keyStore.getKey(VAULT_KEY_ALIAS, null) as? SecretKey
      if (key == null) {
        invoke.resolve()
        return
      }
      val cipher = Cipher.getInstance("AES/GCM/NoPadding")
      cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, Base64.decode(iv, Base64.NO_WRAP)))
      val value = String(cipher.doFinal(Base64.decode(encrypted, Base64.NO_WRAP)))
      invoke.resolve(app.tauri.plugin.JSObject().apply { put("value", value) })
    } catch (_: Exception) {
      clearStoredVaultKey()
      invoke.resolve()
    }
  }

  @Command
  fun clearVaultKey(invoke: Invoke) {
    clearStoredVaultKey()
    invoke.resolve()
  }

  @Command
  fun notifyDownload(invoke: Invoke) {
    val args = invoke.parseArgs(DownloadNotificationArgs::class.java)
    val manager = NotificationManagerCompat.from(activity)
    if (
      Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
      ActivityCompat.checkSelfPermission(activity, Manifest.permission.POST_NOTIFICATIONS) !=
        PackageManager.PERMISSION_GRANTED
    ) {
      invoke.resolve()
      return
    }

    val finished = args.status == "ready" || args.status == "failed"
    val title = when (args.status) {
      "ready" -> "Package сохранён"
      "failed" -> "Ошибка сохранения"
      else -> "Сохранение package"
    }
    val notification = NotificationCompat.Builder(activity, DOWNLOAD_CHANNEL)
      .setSmallIcon(activity.applicationInfo.icon)
      .setContentTitle(title)
      .setContentText(args.message)
      .setOnlyAlertOnce(true)
      .setOngoing(!finished)
      .setAutoCancel(finished)
    if (finished) {
      notification.setProgress(0, 0, false)
    } else {
      notification.setProgress(100, (args.progress.coerceIn(0.0, 1.0) * 100).toInt(), false)
    }
    manager.notify(DOWNLOAD_NOTIFICATION_ID, notification.build())
    invoke.resolve()
  }

  @Command
  fun open(invoke: Invoke) {
    val args = invoke.parseArgs(OpenNodeArgs::class.java)
    val nodeUri = Uri.parse(args.nodeUrl)
    if (
      (nodeUri.scheme != "https" && nodeUri.scheme != "http") ||
      nodeUri.host.isNullOrEmpty() ||
      args.cookieName !in setOf("access_token", "netsanctum_offline") ||
      args.cookieValue.isEmpty() ||
      args.cookieValue.any { it == ';' || it == '\r' || it == '\n' }
    ) {
      invoke.reject("Invalid node session")
      return
    }

    activity.runOnUiThread {
      val dialog = Dialog(activity, android.R.style.Theme_DeviceDefault_NoActionBar)
      val container = FrameLayout(activity)
      container.setBackgroundColor(Color.BLACK)
      val content = LinearLayout(activity).apply {
        orientation = LinearLayout.VERTICAL
        setBackgroundColor(Color.BLACK)
      }
      val toolbar = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(4), 0, dp(4), 0)
        setBackgroundColor(Color.rgb(5, 8, 7))
      }
      val webView = WebView(activity)
      webView.setBackgroundColor(Color.BLACK)
      val homeButton = toolbarButton("NS")
      val backButton = toolbarButton("<")
      val moduleButton = toolbarButton("MODULE").apply {
        maxLines = 1
        ellipsize = TextUtils.TruncateAt.END
      }
      val languageButton = toolbarButton("LANG")
      val settingsButton = toolbarButton("SET")
      toolbar.addView(homeButton)
      toolbar.addView(backButton)
      toolbar.addView(moduleButton, LinearLayout.LayoutParams(0, dp(44), 1f))
      toolbar.addView(languageButton)
      toolbar.addView(settingsButton)

      val page = FrameLayout(activity)
      page.addView(
        webView,
        FrameLayout.LayoutParams(
          ViewGroup.LayoutParams.MATCH_PARENT,
          ViewGroup.LayoutParams.MATCH_PARENT,
        ),
      )
      val loading = ProgressBar(activity)
      page.addView(
        loading,
        FrameLayout.LayoutParams(dp(40), dp(40), Gravity.CENTER),
      )
      content.addView(toolbar, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(48)))
      content.addView(page, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))

      val contentLayout = FrameLayout.LayoutParams(
        ViewGroup.LayoutParams.MATCH_PARENT,
        ViewGroup.LayoutParams.MATCH_PARENT,
      )
      container.addView(content, contentLayout)
      ViewCompat.setOnApplyWindowInsetsListener(container) { _, insets ->
        val safeInsets = insets.getInsets(
          WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout(),
        )
        contentLayout.setMargins(safeInsets.left, safeInsets.top, safeInsets.right, safeInsets.bottom)
        content.layoutParams = contentLayout
        insets
      }
      homeButton.setOnClickListener { dialog.dismiss() }
      backButton.setOnClickListener {
        if (webView.canGoBack()) webView.goBack() else dialog.dismiss()
      }
      settingsButton.setOnClickListener { showSettings(settingsButton, webView) }
      webView.settings.apply {
        javaScriptEnabled = true
        domStorageEnabled = true
        useWideViewPort = true
        loadWithOverviewMode = false
        textZoom = 100
        allowFileAccess = false
        allowContentAccess = false
        javaScriptCanOpenWindowsAutomatically = false
        setSupportMultipleWindows(false)
        mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
      }
      val preferences = activity.getSharedPreferences("netsanctum_ui", Activity.MODE_PRIVATE)
      webView.settings.textZoom = preferences.getInt("text_zoom", 100)
      webView.keepScreenOn = preferences.getBoolean("keep_screen_on", false)
      webView.webViewClient = object : WebViewClient() {
        override fun onPageStarted(view: WebView, url: String, favicon: android.graphics.Bitmap?) {
          super.onPageStarted(view, url, favicon)
          loading.visibility = android.view.View.VISIBLE
        }

        override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
          if (request.isForMainFrame && isDownloadRequest(request.url)) {
            request.url.getQueryParameter("manifest")?.let { requestDownload(it) }
            return true
          }
          if (!request.isForMainFrame || sameOrigin(nodeUri, request.url)) return false
          activity.startActivity(Intent(Intent.ACTION_VIEW, request.url))
          return true
        }

        override fun onPageFinished(view: WebView, url: String) {
          super.onPageFinished(view, url)
          loading.visibility = android.view.View.GONE
          if (
            args.cookieName == "access_token" &&
            sameOrigin(nodeUri, Uri.parse(url))
          ) {
            installDownloadBridge(view, nodeUri)
            installNativeNavigation(view, nodeUri, moduleButton, languageButton)
          } else if (
            args.cookieName == "netsanctum_offline" &&
            sameOrigin(nodeUri, Uri.parse(url))
          ) {
            installOfflineStyles(view)
            moduleButton.text = "OFFLINE"
            moduleButton.isEnabled = false
            languageButton.visibility = android.view.View.GONE
          }
        }

        override fun onReceivedError(
          view: WebView,
          request: WebResourceRequest,
          error: WebResourceError,
        ) {
          super.onReceivedError(view, request, error)
          if (request.isForMainFrame) {
            val message = TextUtils.htmlEncode("Не удалось открыть узел: ${error.description}")
            view.loadDataWithBaseURL(
              null,
              "<html><body style='margin:0;padding:32px;background:#0a0d0b;color:#e9eee8;font-family:monospace'><h2>NetSanctum</h2><p>$message</p></body></html>",
              "text/html",
              "UTF-8",
              null,
            )
          }
        }
      }

      val cookies = CookieManager.getInstance()
      cookies.setAcceptCookie(true)
      cookies.setAcceptThirdPartyCookies(webView, false)
      val secure = if (nodeUri.scheme == "https") "; Secure" else ""
      cookies.setCookie(
        args.nodeUrl,
        "${args.cookieName}=${args.cookieValue}; Path=/; HttpOnly; SameSite=Lax$secure",
      )
      cookies.flush()

      dialog.setContentView(container)
      dialog.setOnKeyListener { _, keyCode, event ->
        if (keyCode != KeyEvent.KEYCODE_BACK || event.action != KeyEvent.ACTION_UP) {
          false
        } else if (webView.canGoBack()) {
          webView.goBack()
          true
        } else {
          dialog.dismiss()
          true
        }
      }
      dialog.setOnDismissListener {
        cookies.setCookie(args.nodeUrl, "${args.cookieName}=; Path=/; Max-Age=0$secure")
        webView.stopLoading()
        webView.destroy()
      }
      dialog.show()
      dialog.window?.let { window ->
        WindowCompat.setDecorFitsSystemWindows(window, false)
        window.setLayout(
          ViewGroup.LayoutParams.MATCH_PARENT,
          ViewGroup.LayoutParams.MATCH_PARENT,
        )
      }
      ViewCompat.requestApplyInsets(container)
      webView.loadUrl(args.nodeUrl)
    }
    invoke.resolve()
  }

  private fun installDownloadBridge(webView: WebView, nodeUri: Uri) {
    val origin = "${nodeUri.scheme}://${nodeUri.encodedAuthority}"
    val script = """
      (() => {
        if (location.origin !== ${org.json.JSONObject.quote(origin)} || window.__NETSANCTUM_DESKTOP__) return;
        Object.defineProperty(window, "__NETSANCTUM_DESKTOP__", {
          value: Object.freeze({
            version: 1,
            requestDownload(manifestUrl) {
              const manifest = new URL(manifestUrl, location.href);
              if (manifest.origin !== location.origin) throw new Error("Manifest must use the node origin");
              const bridge = new URL("https://mobile.netsanctum.invalid/download-package");
              bridge.searchParams.set("manifest", manifest.pathname + manifest.search);
              location.href = bridge.href;
            }
          }),
          configurable: false,
          writable: false
        });
        document.documentElement.classList.add("has-outpost-bridge");
        document.querySelector('body > nav')?.style.setProperty('display', 'none', 'important');
      })();
    """.trimIndent()
    webView.evaluateJavascript(script, null)
  }

  private fun installOfflineStyles(webView: WebView) {
    val script = """
      (() => {
        let viewport = document.querySelector('meta[name="viewport"]');
        if (!viewport) {
          viewport = document.createElement('meta');
          viewport.name = 'viewport';
          document.head.appendChild(viewport);
        }
        viewport.content = 'width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no';
        document.querySelector('body > nav')?.style.setProperty('display', 'none', 'important');
        if (document.getElementById('netsanctum-mobile-offline')) return;
        const style = document.createElement('style');
        style.id = 'netsanctum-mobile-offline';
        style.textContent = `
          html, body { width: 100% !important; max-width: 100vw !important; min-width: 0 !important; overflow-x: hidden !important; }
          #main-content { width: 100% !important; max-width: 100% !important; min-width: 0 !important; padding: 0.75rem !important; }
          img, video, canvas, svg { max-width: 100% !important; }
          #player-grid-wrapper, #playlist-player-wrapper { display: flex !important; flex-direction: column !important; width: 100% !important; min-width: 0 !important; padding: 0.5rem !important; gap: 0.75rem !important; }
          #player-main-col, #player-side-col { width: 100% !important; max-width: 100% !important; min-width: 0 !important; position: static !important; }
          .aspect-video { width: 100% !important; max-width: 100% !important; height: auto !important; min-height: 0 !important; aspect-ratio: 16 / 9 !important; }
          .custom-video-player, .custom-video-player video { width: 100% !important; height: 100% !important; object-fit: contain !important; }
          .custom-controls { padding: 0.5rem !important; }
          .custom-controls .gap-4 { gap: 0.5rem !important; }
          .settings-menu-container, .vol-slider { display: none !important; }
        `;
        document.head.appendChild(style);
      })();
    """.trimIndent()
    webView.evaluateJavascript(script, null)
  }

  private fun installNativeNavigation(
    webView: WebView,
    nodeUri: Uri,
    moduleButton: Button,
    languageButton: Button,
  ) {
    val script = """
      (() => {
        const selects = [...document.querySelectorAll('body > nav select')];
        const languages = document.querySelector('body > nav select[name="lang"]');
        const modules = selects.find(select => select !== languages && [...select.options].some(option => option.value.startsWith('/')));
        document.querySelector('body > nav')?.style.setProperty('display', 'none', 'important');
        return JSON.stringify({
          modules: modules ? [...modules.options].filter(option => option.value.startsWith('/') && !option.value.startsWith('/set-language')).map(option => ({ label: option.textContent.trim(), path: option.value, selected: option.selected })) : [],
          language: languages?.selectedOptions[0]?.textContent.trim().toUpperCase() || 'EN'
        });
      })();
    """.trimIndent()
    webView.evaluateJavascript(script) { raw ->
      try {
        val encoded = org.json.JSONTokener(raw).nextValue() as String
        val navigation = org.json.JSONObject(encoded)
        val modules = navigation.getJSONArray("modules")
        val labels = ArrayList<String>()
        val paths = ArrayList<String>()
        var selected = 0
        for (index in 0 until modules.length()) {
          val module = modules.getJSONObject(index)
          val path = module.getString("path")
          if (!validRelativeUrl(path)) continue
          labels.add(module.getString("label"))
          paths.add(path)
          if (module.optBoolean("selected")) selected = labels.lastIndex
        }
        if (labels.isNotEmpty()) {
          moduleButton.text = labels[selected]
          moduleButton.isEnabled = true
          moduleButton.setOnClickListener {
            showChoicePopup(moduleButton, labels, selected) { index ->
              webView.loadUrl("${nodeUri.scheme}://${nodeUri.encodedAuthority}${paths[index]}")
            }
          }
        }
        val currentLanguage = navigation.optString("language", "EN")
        languageButton.text = currentLanguage
        languageButton.setOnClickListener {
          val languages = listOf("RU", "EN")
          showChoicePopup(languageButton, languages, languages.indexOf(currentLanguage)) { index ->
            setLanguage(webView, if (index == 0) "ru" else "en")
          }
        }
      } catch (_: Exception) {
        moduleButton.text = "MODULE"
      }
    }
  }

  private fun setLanguage(webView: WebView, language: String) {
    val script = """
      fetch('/set-language', {
        method: 'POST',
        credentials: 'same-origin',
        headers: {'Content-Type': 'application/x-www-form-urlencoded'},
        body: 'lang=$language&next_url=' + encodeURIComponent(location.pathname + location.search)
      }).then(() => location.reload());
    """.trimIndent()
    webView.evaluateJavascript(script, null)
  }

  private fun showSettings(anchor: Button, webView: WebView) {
    val preferences = activity.getSharedPreferences("netsanctum_ui", Activity.MODE_PRIVATE)
    val scales = listOf(90, 100, 115, 130)
    val currentScale = preferences.getInt("text_zoom", 100)
    val keepScreenOn = preferences.getBoolean("keep_screen_on", false)
    val choices = scales.map { "ТЕКСТ $it%" } +
      listOf("ЭКРАН: ${if (keepScreenOn) "НЕ ГАСИТЬ" else "ОБЫЧНО"}", "ОЧИСТИТЬ WEB-КЭШ")
    showChoicePopup(anchor, choices, scales.indexOf(currentScale)) { index ->
      when {
        index < scales.size -> {
          val scale = scales[index]
          preferences.edit().putInt("text_zoom", scale).apply()
          webView.settings.textZoom = scale
        }
        index == scales.size -> {
          val enabled = !keepScreenOn
          preferences.edit().putBoolean("keep_screen_on", enabled).apply()
          webView.keepScreenOn = enabled
        }
        else -> webView.clearCache(true)
      }
    }
  }

  private fun showChoicePopup(
    anchor: Button,
    choices: List<String>,
    selected: Int,
    onSelect: (Int) -> Unit,
  ) {
    val panel = LinearLayout(activity).apply {
      orientation = LinearLayout.VERTICAL
      setPadding(dp(1), dp(1), dp(1), dp(1))
      background = GradientDrawable().apply {
        setColor(Color.rgb(7, 13, 11))
        setStroke(dp(1), Color.rgb(45, 212, 191))
      }
    }
    val popup = PopupWindow(
      panel,
      maxOf(anchor.width, dp(210)),
      ViewGroup.LayoutParams.WRAP_CONTENT,
      true,
    ).apply {
      isOutsideTouchable = true
      elevation = dp(8).toFloat()
      setBackgroundDrawable(GradientDrawable().apply { setColor(Color.rgb(7, 13, 11)) })
    }
    choices.forEachIndexed { index, label ->
      panel.addView(TextView(activity).apply {
        text = label
        setTextColor(if (index == selected) Color.rgb(45, 212, 191) else Color.rgb(190, 201, 194))
        setBackgroundColor(if (index == selected) Color.rgb(13, 35, 29) else Color.TRANSPARENT)
        textSize = 12f
        setPadding(dp(14), dp(13), dp(14), dp(13))
        setOnClickListener {
          popup.dismiss()
          onSelect(index)
        }
      })
    }
    popup.showAsDropDown(anchor, 0, dp(2))
  }

  private fun toolbarButton(label: String): Button = Button(activity).apply {
    text = label
    isAllCaps = false
    setTextColor(Color.rgb(45, 212, 191))
    setBackgroundColor(Color.TRANSPARENT)
    textSize = 11f
    minWidth = 0
    minimumWidth = 0
    setPadding(dp(10), 0, dp(10), 0)
  }

  private fun validRelativeUrl(value: String): Boolean {
    val uri = Uri.parse(value)
    return value.length <= 4096 &&
      value.startsWith("/") &&
      !value.startsWith("//") &&
      uri.scheme == null &&
      uri.authority == null &&
      uri.fragment == null
  }

  private fun validId(value: String): Boolean =
    value.isNotEmpty() && value.length <= 160 && value.all {
      it.isLetterOrDigit() || it == '_' || it == '-' || it == '.'
    }

  private fun captureShortcut(intent: Intent) {
    if (intent.action != ACTION_OPEN_OFFLINE_MODULE) return
    val nodeOrigin = intent.getStringExtra(EXTRA_NODE_ORIGIN) ?: return
    val moduleId = intent.getStringExtra(EXTRA_MODULE_ID) ?: return
    val nodeUri = Uri.parse(nodeOrigin)
    if (
      (nodeUri.scheme != "https" && nodeUri.scheme != "http") ||
      nodeUri.host.isNullOrEmpty() ||
      !validId(moduleId)
    ) return
    pendingShortcutNode = nodeOrigin
    pendingShortcutModule = moduleId
    intent.action = null
  }

  private fun dispatchPendingShortcut() {
    val nodeOrigin = pendingShortcutNode ?: return
    val moduleId = pendingShortcutModule ?: return
    val payload = org.json.JSONObject().apply {
      put("nodeOrigin", nodeOrigin)
      put("moduleId", moduleId)
    }
    pendingShortcutNode = null
    pendingShortcutModule = null
    shellWebView?.post {
      shellWebView?.evaluateJavascript(
        "window.dispatchEvent(new CustomEvent('netsanctum-open-shortcut',{detail:$payload}))",
        null,
      )
    }
  }

  private fun shortcutIcon(moduleId: String): Bitmap {
    val size = 192
    val bitmap = Bitmap.createBitmap(size, size, Bitmap.Config.ARGB_8888)
    val canvas = Canvas(bitmap)
    val paint = Paint(Paint.ANTI_ALIAS_FLAG)
    paint.color = Color.rgb(5, 13, 10)
    canvas.drawRect(0f, 0f, size.toFloat(), size.toFloat(), paint)
    paint.style = Paint.Style.STROKE
    paint.strokeWidth = 8f
    paint.color = Color.rgb(45, 212, 191)
    canvas.drawRect(12f, 12f, size - 12f, size - 12f, paint)
    paint.style = Paint.Style.FILL
    paint.typeface = Typeface.create(Typeface.MONOSPACE, Typeface.BOLD)
    paint.textAlign = Paint.Align.CENTER
    paint.textSize = 58f
    val initials = moduleId.split('_', '-').mapNotNull { it.firstOrNull() }.take(2)
      .joinToString("").uppercase().ifEmpty { "NS" }
    val baseline = size / 2f - (paint.ascent() + paint.descent()) / 2f
    canvas.drawText(initials, size / 2f, baseline, paint)
    return bitmap
  }

  private fun getOrCreateVaultKey(): SecretKey {
    val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    (keyStore.getKey(VAULT_KEY_ALIAS, null) as? SecretKey)?.let { return it }
    return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").run {
      init(
        KeyGenParameterSpec.Builder(
          VAULT_KEY_ALIAS,
          KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
          .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
          .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
          .build(),
      )
      generateKey()
    }
  }

  private fun clearStoredVaultKey() {
    activity.getSharedPreferences(VAULT_PREFS, Activity.MODE_PRIVATE).edit().clear().apply()
    val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    if (keyStore.containsAlias(VAULT_KEY_ALIAS)) keyStore.deleteEntry(VAULT_KEY_ALIAS)
  }

  private fun dp(value: Int): Int =
    (value * activity.resources.displayMetrics.density).toInt()

  private fun isDownloadRequest(uri: Uri): Boolean =
    uri.scheme == "https" &&
      uri.host == "mobile.netsanctum.invalid" &&
      uri.path == "/download-package"

  private fun requestDownload(manifestUrl: String) {
    val manifest = Uri.parse(manifestUrl)
    if (
      manifestUrl.length > 4096 ||
      !manifestUrl.startsWith("/") ||
      manifestUrl.startsWith("//") ||
      manifest.scheme != null ||
      manifest.authority != null ||
      manifest.fragment != null
    ) {
      Toast.makeText(activity, "Некорректный manifest", Toast.LENGTH_SHORT).show()
      return
    }
    requestNotificationPermission()
    val quotedManifest = org.json.JSONObject.quote(manifestUrl)
    shellWebView?.post {
      shellWebView?.evaluateJavascript(
        "window.dispatchEvent(new CustomEvent('netsanctum-download-request',{detail:$quotedManifest}))",
        null,
      )
    }
    Toast.makeText(activity, "Загрузка package начата", Toast.LENGTH_SHORT).show()
  }

  private fun requestNotificationPermission() {
    if (
      Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
      !notificationPermissionRequested &&
      ActivityCompat.checkSelfPermission(activity, Manifest.permission.POST_NOTIFICATIONS) !=
        PackageManager.PERMISSION_GRANTED
    ) {
      notificationPermissionRequested = true
      ActivityCompat.requestPermissions(
        activity,
        arrayOf(Manifest.permission.POST_NOTIFICATIONS),
        NOTIFICATION_PERMISSION_REQUEST,
      )
    }
  }

  private fun createNotificationChannel() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
    val channel = NotificationChannel(
      DOWNLOAD_CHANNEL,
      "Загрузки NetSanctum",
      NotificationManager.IMPORTANCE_LOW,
    )
    activity.getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
  }

  companion object {
    private const val DOWNLOAD_CHANNEL = "netsanctum_downloads"
    private const val DOWNLOAD_NOTIFICATION_ID = 4101
    private const val NOTIFICATION_PERMISSION_REQUEST = 4102
    private const val ACTION_OPEN_OFFLINE_MODULE = "dev.netsanctum.desktop.OPEN_OFFLINE_MODULE"
    private const val EXTRA_NODE_ORIGIN = "node_origin"
    private const val EXTRA_MODULE_ID = "module_id"
    private const val VAULT_PREFS = "netsanctum_vault_key"
    private const val VAULT_KEY_ALIAS = "netsanctum_vault_key_v1"
  }

  private fun sameOrigin(expected: Uri, actual: Uri): Boolean {
    fun effectivePort(uri: Uri): Int = when {
      uri.port != -1 -> uri.port
      uri.scheme == "https" -> 443
      else -> 80
    }
    return expected.scheme == actual.scheme &&
      expected.host == actual.host &&
      effectivePort(expected) == effectivePort(actual)
  }
}
