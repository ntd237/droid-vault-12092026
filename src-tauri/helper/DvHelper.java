// DvHelper — on-device label/icon resolver for Droid Vault (hướng 2).
//
// Run by Droid Vault through: `CLASSPATH=/data/local/tmp/dv_helper.dex
//   app_process /system/bin DvHelper` as the adb shell user.
//
// Protocol (stdin/stdout, one line per package):
//   stdin : package name per line, UTF-8; EOF ends the helper
//   stdout: `OK\t<pkg>\t<label_b64>\t<icon_png_b64 | ->`   (tab-separated)
//           `ERR\t<pkg>\t<reason_b64>`
//   each line is flushed immediately so the host can stream results.
//
// Android 9..16 compatibility:
// - Package info comes from the RAW IPackageManager binder (AppGlobals
//   .getPackageManager()) — the public PackageManager wrapper rejects shell
//   uid with "Given calling package android does not belong to uid 2000".
//   The binder-level getApplicationInfo signature changed int→long flags in
//   Android 13; both shapes are tried reflectively.
// - MATCH_UNINSTALLED_PACKAGES keeps hidden (uninstalled-for-user-0) packages
//   resolvable — Droid Vault's restore list needs their labels/icons too.
// - Label/icon are loaded from the package's OWN Resources via
//   createPackageContext(CONTEXT_IGNORE_SECURITY) — no attribution involved.
// - If any of the reflective entry points disappears in a future Android, the
//   helper fails fast and the host falls back to pulling APKs (existing
//   pipeline), so this helper is an accelerator, never a hard dependency.
//
// Build: helper/build.sh (javac --release 8 against android.jar, d8 --min-api 28).

import android.content.Context;
import android.content.pm.ApplicationInfo;
import android.content.res.Resources;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.drawable.Drawable;
import android.os.Looper;
import android.util.Base64;

import java.io.BufferedReader;
import java.io.ByteArrayOutputStream;
import java.io.InputStreamReader;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;

public class DvHelper {

    private static final int ICON_SIZE = 192;
    // PackageManager.MATCH_UNINSTALLED_PACKAGES — literal: the field lives on
    // the wrapper class we deliberately avoid touching at compile time too.
    private static final long MATCH_UNINSTALLED_PACKAGES = 8192L;
    private static final int CREATE_FLAGS =
            Context.CONTEXT_IGNORE_SECURITY | Context.CONTEXT_INCLUDE_CODE;

    private static Context systemContext() throws Exception {
        // system_server runs Looper.prepareMainLooper() before systemMain();
        // ActivityThread's constructor needs a prepared looper for its Handler.
        Looper.prepareMainLooper();
        Class<?> at = Class.forName("android.app.ActivityThread");
        Object thread = at.getDeclaredMethod("systemMain").invoke(null);
        Method getSystemContext = at.getDeclaredMethod("getSystemContext");
        return (Context) getSystemContext.invoke(thread);
    }

    private static Object rawPackageManager() throws Exception {
        Class<?> globals = Class.forName("android.app.AppGlobals");
        return globals.getMethod("getPackageManager").invoke(null);
    }

    /** Binder-level getApplicationInfo; flags widened to long in Android 13. */
    private static ApplicationInfo appInfo(Object rawPm, String pkg) throws Exception {
        for (Class<?> flagType : new Class<?>[] { long.class, int.class }) {
            try {
                Method m = rawPm.getClass().getMethod(
                        "getApplicationInfo", String.class, flagType, int.class);
                Object flags = flagType == long.class
                        ? (Object) MATCH_UNINSTALLED_PACKAGES
                        : (Object) (int) MATCH_UNINSTALLED_PACKAGES;
                return (ApplicationInfo) m.invoke(rawPm, pkg, flags, 0);
            } catch (NoSuchMethodException ignored) {
                // try the other flag width
            }
        }
        throw new IllegalStateException("getApplicationInfo not found on IPackageManager");
    }

    private static String b64(String s) {
        return Base64.encodeToString(s.getBytes(StandardCharsets.UTF_8), Base64.NO_WRAP);
    }

    private static String iconPng(Resources res, ApplicationInfo info) throws Exception {
        int id = info.icon != 0 ? info.icon : info.logo;
        if (id == 0) {
            return "-";
        }
        Drawable d = res.getDrawable(id, null);
        if (d == null) {
            return "-";
        }
        Bitmap bmp = Bitmap.createBitmap(ICON_SIZE, ICON_SIZE, Bitmap.Config.ARGB_8888);
        Canvas canvas = new Canvas(bmp);
        d.setBounds(0, 0, ICON_SIZE, ICON_SIZE);
        d.draw(canvas);
        ByteArrayOutputStream png = new ByteArrayOutputStream();
        bmp.compress(Bitmap.CompressFormat.PNG, 100, png);
        bmp.recycle();
        return Base64.encodeToString(png.toByteArray(), Base64.NO_WRAP);
    }

    // H1: workers resolve packages concurrently (icon render + binder calls
    // are CPU-bound; 13.5s serial was the cold-scan bottleneck). Output lines
    // are tagged per package so the host does not depend on ordering.
    private static final Object OUT_LOCK = new Object();

    private static void emit(String line) {
        synchronized (OUT_LOCK) {
            System.out.println(line);
            System.out.flush();
        }
    }

    private static void resolveOne(Context sysCtx, Object rawPm, String pkg) {
        try {
            ApplicationInfo info = appInfo(rawPm, pkg);
            if (info == null) {
                throw new IllegalStateException("package not found");
            }
            Context pctx = sysCtx.createPackageContext(pkg, CREATE_FLAGS);
            String label = info.nonLocalizedLabel != null
                    ? info.nonLocalizedLabel.toString()
                    : pctx.getResources().getString(info.labelRes);
            String iconB64 = iconPng(pctx.getResources(), info);
            emit("OK\t" + pkg + "\t" + b64(label) + "\t" + iconB64);
        } catch (Throwable t) {
            emit("ERR\t" + pkg + "\t" + b64(String.valueOf(t)));
        }
    }

    public static void main(String[] args) throws Exception {
        Context sysCtx = systemContext();
        Object rawPm = rawPackageManager();

        // Small-core count with headroom for the big cores; capped so many
        // concurrent binder calls cannot starve system_server.
        int threads = Math.max(2, Math.min(8, Runtime.getRuntime().availableProcessors()));
        java.util.concurrent.ExecutorService pool =
                java.util.concurrent.Executors.newFixedThreadPool(threads);

        BufferedReader in = new BufferedReader(
                new InputStreamReader(System.in, StandardCharsets.UTF_8));
        String pkg;
        while ((pkg = in.readLine()) != null) {
            final String p = pkg.trim();
            if (p.isEmpty()) {
                continue;
            }
            pool.submit(() -> resolveOne(sysCtx, rawPm, p));
        }
        pool.shutdown();
        pool.awaitTermination(10, java.util.concurrent.TimeUnit.MINUTES);
    }
}
