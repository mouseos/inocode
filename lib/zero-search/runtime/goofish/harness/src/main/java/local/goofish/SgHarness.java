package local.goofish;

import com.github.unidbg.AndroidEmulator;
import com.github.unidbg.Module;
import com.github.unidbg.linux.android.AndroidEmulatorBuilder;
import com.github.unidbg.linux.android.AndroidResolver;
import com.github.unidbg.linux.android.dvm.*;
import com.github.unidbg.linux.android.dvm.array.ArrayObject;
import com.github.unidbg.memory.Memory;

import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.StandardCopyOption;
import java.security.MessageDigest;
import java.util.HashMap;
import java.util.Map;
import java.util.zip.ZipFile;

/**
 * Stage-driven harness:
 *   load   : create emulator + DalvikVM from the real APK, load libsgmainso, run JNI_OnLoad
 *   init   : + doCommandNative(10101, ...) SG plugin init
 *   appkey : + doCommandNative(10602, [0, ""]) getAppKeyByIndex
 *   sign   : + doCommandNative(10401, [ {INPUT=...}, appKey, 7, authCode, TRUE ]) signRequest
 *   sign2  : sign twice with same INPUT (reproducibility), then once with different INPUT
 */
public class SgHarness extends AbstractJni {

    static final String PKG = "com.taobao.idlefish";
    static final File APK = new File(System.getProperty("apk",
            System.getProperty("user.home") + "/workspace/goofish-analysis/apk/idlefish_7.28.70_yyb.apk"));
    static final File SO = new File(System.getProperty("so",
            "libs/libsgmainso-6.7.260202.so"));
    static final boolean APK_EXPLICIT = System.getProperty("apk") != null;
    static final String PUBLIC_APPKEY = "21407387";

    private final AndroidEmulator emulator;
    private final VM vm;
    private DvmClass jnicLibrary;

    // ---- Round29: APK/SO entity validation + staging (runs before emulator) ----
    // Root cause: the native SG library does not read -Dapk; via the JNI callback
    // Context.getPackageCodePath() it always opens the fixed guest path
    // /data/app/com.taobao.idlefish-1/base.apk, which unidbg resolves to the host
    // file rootfs/data/app/com.taobao.idlefish-1/base.apk. Therefore validate the
    // supplied APK, then copy it onto that staged location so that the file the
    // native code actually opens IS the explicitly supplied file.
    static final File STAGED_APK = new File("rootfs/data/app/" + PKG + "-1/base.apk");

    private static void failValidation(String msg) {
        System.err.println("[harness][fatal] " + msg);
        System.err.flush();
        System.exit(2);
        throw new IllegalStateException(msg); // unreachable; satisfies compiler
    }

    private static String sha256Hex(File f) throws Exception {
        MessageDigest md = MessageDigest.getInstance("SHA-256");
        byte[] buf = new byte[1024 * 1024];
        InputStream in = new FileInputStream(f);
        try {
            int n;
            while ((n = in.read(buf)) != -1) md.update(buf, 0, n);
        } finally {
            in.close();
        }
        byte[] d = md.digest();
        StringBuilder sb = new StringBuilder(d.length * 2);
        for (byte b : d) sb.append(String.format("%02x", b & 0xff));
        return sb.toString();
    }

    private static String canonicalPath(File f) {
        try { return f.getCanonicalPath(); } catch (IOException e) { return f.getAbsolutePath(); }
    }

    private static void validateAndStageInputs() {
        // ---- APK: existence / regular file / readable ----
        String apkAbs = canonicalPath(APK);
        String apkSource = APK_EXPLICIT ? "explicit" : "default";
        if (!APK.exists()) {
            failValidation("apk not found: path=" + apkAbs + " source=" + apkSource);
        }
        if (!APK.isFile()) {
            failValidation("apk is not a regular file: path=" + apkAbs + " source=" + apkSource);
        }
        if (!APK.canRead()) {
            failValidation("apk is not readable: path=" + apkAbs + " source=" + apkSource);
        }
        // ---- APK: ZIP validity + AndroidManifest.xml presence ----
        ZipFile zf = null;
        try {
            zf = new ZipFile(APK);
            if (zf.getEntry("AndroidManifest.xml") == null) {
                failValidation("apk missing AndroidManifest.xml entry (not an APK): path=" + apkAbs);
            }
        } catch (IOException e) {
            failValidation("apk is not a valid ZIP/APK: path=" + apkAbs + " reason=" + e.getMessage());
        } finally {
            if (zf != null) { try { zf.close(); } catch (IOException ignored) {} }
        }
        // ---- APK: SHA-256 + diagnostic line ----
        String apkSha;
        try {
            apkSha = sha256Hex(APK);
        } catch (Exception e) {
            failValidation("apk sha256 computation failed: path=" + apkAbs + " reason=" + e.getMessage());
            return; // unreachable
        }
        long apkSize = APK.length();
        System.err.println("apk path=" + apkAbs + " size=" + apkSize + " sha256=" + apkSha + " source=" + apkSource);
        System.err.flush();
        // ---- APK: optional expected SHA-256 ----
        String expected = System.getProperty("apkSha256");
        if (expected != null && !expected.trim().isEmpty()) {
            String expNorm = expected.trim().toLowerCase();
            if (!expNorm.equals(apkSha)) {
                failValidation("apk sha256 mismatch: path=" + apkAbs + " expected=" + expNorm + " actual=" + apkSha);
            }
            System.err.println("apk sha256 verified: expected=" + expNorm);
            System.err.flush();
        }
        // ---- SO: existence / regular file / readable + diagnostic line ----
        String soAbs = canonicalPath(SO);
        if (!SO.exists()) {
            failValidation("so not found: path=" + soAbs);
        }
        if (!SO.isFile()) {
            failValidation("so is not a regular file: path=" + soAbs);
        }
        if (!SO.canRead()) {
            failValidation("so is not readable: path=" + soAbs);
        }
        String soSha;
        try {
            soSha = sha256Hex(SO);
        } catch (Exception e) {
            failValidation("so sha256 computation failed: path=" + soAbs + " reason=" + e.getMessage());
            return; // unreachable
        }
        System.err.println("so path=" + soAbs + " sha256=" + soSha);
        System.err.flush();
        // ---- Stage the verified APK onto the rootfs base.apk location ----
        try {
            File apkCanon = APK.getCanonicalFile();
            File stagedCanon = STAGED_APK.getCanonicalFile();
            if (apkCanon.equals(stagedCanon)) {
                System.err.println("apk staged path=" + stagedCanon.getPath() + " sha256=" + apkSha + " copied=false");
                System.err.flush();
            } else {
                File parent = stagedCanon.getParentFile();
                if (parent != null && !parent.exists()) {
                    parent.mkdirs();
                }
                Files.copy(apkCanon.toPath(), stagedCanon.toPath(), StandardCopyOption.REPLACE_EXISTING);
                String stagedSha = sha256Hex(stagedCanon);
                if (!stagedSha.equals(apkSha)) {
                    failValidation("apk staged sha256 mismatch: staged_path=" + stagedCanon.getPath()
                            + " staged_sha256=" + stagedSha + " source_sha256=" + apkSha);
                }
                System.err.println("apk staged path=" + stagedCanon.getPath() + " sha256=" + stagedSha + " copied=true");
                System.err.flush();
            }
        } catch (IOException e) {
            failValidation("apk staging failed: reason=" + e.getMessage());
        } catch (Exception e) {
            failValidation("apk staging verification failed: reason=" + e.getMessage());
        }
    }

    SgHarness() {
        validateAndStageInputs();
        emulator = AndroidEmulatorBuilder.for64Bit()
                .setProcessName(PKG)
                .setRootDir(new File("rootfs"))
                .addBackendFactory(new com.github.unidbg.arm.backend.Unicorn2Factory(true))
                .build();
        Memory memory = emulator.getMemory();
        memory.setLibraryResolver(new AndroidResolver(23));
        emulator.getSyscallHandler().addIOResolver(
                new com.github.unidbg.file.IOResolver<com.github.unidbg.file.linux.AndroidFileIO>() {
                    @Override
                    public com.github.unidbg.file.FileResult<com.github.unidbg.file.linux.AndroidFileIO> resolve(
                            com.github.unidbg.Emulator<com.github.unidbg.file.linux.AndroidFileIO> emulator,
                            String pathname, int oflags) {
                        System.out.println("[io] resolve path=" + pathname + " oflags=0x" + Integer.toHexString(oflags));
                        return null; // fall through to default resolvers
                    }
                });
        vm = emulator.createDalvikVM(APK);
        vm.setJni(this);
        vm.setVerbose(Boolean.getBoolean("verboseJni"));
        System.out.println("[harness] DalvikVM created from APK: " + APK.getPath());
    }

    void probe() {
        jnicLibrary = vm.resolveClass("com/taobao/wireless/security/adapter/JNICLibrary");
        vm.setVerbose(true);
        System.out.println("[probe] loading with callInit=false");
        DalvikModule dm = vm.loadLibrary(SO, false);
        Module m = dm.getModule();
        System.out.println("[probe] base=0x" + Long.toHexString(m.base));
        for (String sym : new String[]{"JNI_OnLoad", "one_jni_onload", "JNI_OnUnload"}) {
            com.github.unidbg.Symbol s = m.findSymbolByName(sym, false);
            System.out.println("[probe] symbol " + sym + " -> " + (s == null ? "NOT FOUND" : "0x" + Long.toHexString(s.getAddress())));
        }
        System.out.println("[probe] calling JNI_OnLoad manually...");
        try {
            dm.callJNI_OnLoad(emulator);
            System.out.println("[probe] JNI_OnLoad returned normally");
        } catch (Throwable t) {
            System.out.println("[probe] JNI_OnLoad threw: " + t);
        }
        try {
            Object r = doCommand(10101, ctxObject(), intObj(0), strObj(""), strObj(""), strObj(""),
                    strObj(PKG), strObj("7.28.70"), strObj("10"), strObj(PKG), intObj(0),
                    vm.resolveClass("com/alibaba/wireless/security/mainplugin/SecurityGuardMainPlugin").newObject("PLUGIN"));
            System.out.println("[probe] doCommand after manual JNI_OnLoad -> " + describe(r));
        } catch (Throwable t) {
            System.out.println("[probe] doCommand threw: " + t);
        }
    }

    void loadLibrary() {
        // Resolve the JNI bridge class BEFORE loading so that RegisterNatives
        // inside JNI_OnLoad can find it in the VM class map.
        jnicLibrary = vm.resolveClass("com/taobao/wireless/security/adapter/JNICLibrary");
        System.out.println("[stage:load] loading " + SO.getPath());
        // NOTE: loadLibrary(so, true) did NOT complete RegisterNatives in this
        // environment (JNI_OnLoad aborted silently), while loadLibrary(false)
        // + manual callJNI_OnLoad registers doCommandNative correctly
        // (evidence: logs/run_probe.log line 99). Use the manual path.
        DalvikModule dm = vm.loadLibrary(SO, false);
        Module m = dm.getModule();
        dm.callJNI_OnLoad(emulator);
        System.out.println("[stage:load] OK base=0x" + Long.toHexString(m.base) + " size=0x" + Long.toHexString(m.size));
        System.out.println("[stage:load] JNICLibrary class resolved, JNI_OnLoad done");
    }

    Object doCommand(int cmd, DvmObject<?>... args) {
        ArrayObject arr = new ArrayObject(args);
        System.out.println("[docommand] cmd=" + cmd + " argc=" + args.length);
        DvmObject<?> ret = jnicLibrary.callStaticJniMethodObject(emulator,
                "doCommandNative(I[Ljava/lang/Object;)Ljava/lang/Object;", cmd, arr);
        Object v = ret == null ? null : ret.getValue();
        System.out.println("[docommand] cmd=" + cmd + " -> " + (ret == null ? "null"
                : ret.getClass().getSimpleName() + "(" + describe(v) + ")"));
        return v;
    }

    static String describe(Object v) {
        if (v == null) return "null";
        if (v instanceof String) {
            String s = (String) v;
            return "len=" + s.length() + " value=" + (s.length() <= 120 ? s : s.substring(0, 120) + "...");
        }
        return String.valueOf(v);
    }

    // ---------------- command wrappers ----------------

    int sgInit() {
        DvmObject<?> ctx = vm.resolveClass("android/content/Context").newObject("CTX");
        DvmObject<?> plugin = vm.resolveClass("com/alibaba/wireless/security/mainplugin/SecurityGuardMainPlugin").newObject("PLUGIN");
        Object r = doCommand(10101,
                ctx,
                intObj(0),                    // flags
                strObj(""),                   // authCode slot (objArr[1] in onPluginLoaded)
                strObj(""),                   // objArr[2]
                strObj(""),                   // objArr[3]
                strObj(PKG),                  // packageName
                strObj("7.28.70"),            // versionName
                strObj("10"),                 // Build.VERSION.RELEASE
                strObj(PKG),                  // processName
                intObj(0),                    // taobao flag
                plugin);
        return r instanceof Number ? ((Number) r).intValue() : -999;
    }

    String getAppKeyByIndex(int index, String authCode) {
        Object r = doCommand(10602, intObj(index), strObj(authCode));
        return r instanceof String ? (String) r : null;
    }

    String signRequest(String input, String appKey, int requestType, String authCode) {
        Map<String, Object> map = new HashMap<>();
        map.put("INPUT", input);
        DvmObject<?> mapObj = vm.resolveClass("java/util/HashMap").newObject(map);
        Object r = doCommand(10401, mapObj, strObj(appKey), intObj(requestType), strObj(authCode), boolObj(true));
        return r instanceof String ? (String) r : null;
    }

    private StringObject strObj(String s) { return new StringObject(vm, s); }
    private DvmObject<?> intObj(int i) { return vm.resolveClass("java/lang/Integer").newObject(Integer.valueOf(i)); }
    private DvmObject<?> boolObj(boolean b) { return vm.resolveClass("java/lang/Boolean").newObject(Boolean.valueOf(b)); }


    // ---------------- JSON-lines service mode (for Python callers) ----------------
    // Protocol: one JSON object per line on stdin:
    //   {"input":"<INPUT string>", "appKey":"21407387", "requestType":7, "authCode":""}
    // One JSON object per line on stdout:
    //   {"sign":"<hex>"}  or  {"error":"..."}
    // A first line {"ready":true,"appKey":"..."} is emitted after init.
    void serve(java.io.PrintStream proto) throws Exception {
        loadLibrary();
        int rc = sgInit();
        String appKey = getAppKeyByIndex(0, "");
        com.alibaba.fastjson.JSONObject ready = new com.alibaba.fastjson.JSONObject();
        ready.put("ready", true);
        ready.put("initRc", rc);
        ready.put("appKey", appKey);
        proto.println(ready.toJSONString());
        proto.flush();
        java.io.BufferedReader in = new java.io.BufferedReader(new java.io.InputStreamReader(System.in, "UTF-8"));
        String line;
        while ((line = in.readLine()) != null) {
            com.alibaba.fastjson.JSONObject resp = new com.alibaba.fastjson.JSONObject();
            try {
                com.alibaba.fastjson.JSONObject req = com.alibaba.fastjson.JSON.parseObject(line);
                String input = req.getString("input");
                String k = req.getString("appKey");
                if (k == null) k = appKey;
                int rt = req.getIntValue("requestType") == 0 && !req.containsKey("requestType") ? 7 : req.getIntValue("requestType");
                String ac = req.getString("authCode");
                if (ac == null) ac = "";
                String sign = signRequest(input, k, rt, ac);
                if (sign == null) {
                    resp.put("error", "sign returned null (native SecException; see stderr log)");
                } else {
                    resp.put("sign", sign);
                }
            } catch (Throwable t) {
                resp.put("error", String.valueOf(t));
            }
            proto.println(resp.toJSONString());
            proto.flush();
        }
    }

    // ---------------- main ----------------

    public static void main(String[] args) throws Exception {
        String stage = args.length > 0 ? args[0] : "load";
        if (stage.equals("serve")) {
            java.io.PrintStream proto = System.out;
            System.setOut(new java.io.PrintStream(System.err, true));
            SgHarness h = new SgHarness();
            try {
                h.serve(proto);
            } finally {
                h.emulator.close();
            }
            return;
        }
        SgHarness h = new SgHarness();
        try {
            if (stage.equals("probe")) { h.probe(); System.out.println("[result] stage=probe DONE"); return; }
            h.loadLibrary();
            if (stage.equals("load")) { System.out.println("[result] stage=load DONE"); return; }

            int rc = h.sgInit();
            System.out.println("[stage:init] doCommand(10101) returned " + rc);
            if (stage.equals("init")) { System.out.println("[result] stage=init DONE"); return; }

            String appKey = h.getAppKeyByIndex(0, "");
            System.out.println("[stage:appkey] getAppKeyByIndex(0) -> " + describe(appKey));
            if (stage.equals("appkey")) { System.out.println("[result] stage=appkey DONE"); return; }

            String useKey = appKey != null ? appKey : PUBLIC_APPKEY;
            // Fixed synthetic INPUT (format per mtop-signing.md; values are test constants, no real data)
            String input = String.join("&",
                    "testutdid000000", "", "", useKey,
                    "900150983cd24fb0d6963f7d28e17f72", // md5("abc")
                    "1700000000", "mtop.test.api", "1.0", "", "700000@idlefish_android_7.28.70",
                    "testdeviceid0000", "", "", "", "0", "", "", "", "", "", "", "");
            String s1 = h.signRequest(input, useKey, 7, "");
            System.out.println("[stage:sign] sign#1 -> " + describe(s1));
            if (stage.equals("sign2") && s1 != null) {
                String s2 = h.signRequest(input, useKey, 7, "");
                System.out.println("[stage:sign] sign#2 (same input) -> " + describe(s2));
                System.out.println("[stage:sign] reproducible=" + s1.equals(s2));
                String input2 = input.replace("1700000000", "1700000060");
                String s3 = h.signRequest(input2, useKey, 7, "");
                System.out.println("[stage:sign] sign#3 (t+60) -> " + describe(s3));
                System.out.println("[stage:sign] differsOnTimeChange=" + !s1.equals(s3));
            }
            System.out.println("[result] stage=" + stage + " DONE");
        } finally {
            h.emulator.close();
        }
    }

    // ---------------- JNI callbacks ----------------
    // Minimal Android environment emulation. Everything unhandled is logged loudly.

    private DvmObject<?> ctxObject() {
        return vm.resolveClass("android/content/Context").newObject("CTX");
    }

    @Override
    public DvmObject<?> callStaticObjectMethod(BaseVM vm, DvmClass dvmClass, String signature, VarArg vaList) {
        return staticObjCore(vm, dvmClass, signature, vaList);
    }

    @Override
    public DvmObject<?> callStaticObjectMethodV(BaseVM vm, DvmClass dvmClass, String signature, VaList vaList) {
        return staticObjCore(vm, dvmClass, signature, vaList);
    }

    public DvmObject<?> staticObjCore(BaseVM vm, DvmClass dvmClass, String signature, VarArg vaList) {
        switch (signature) {
            case "com/alibaba/wireless/security/mainplugin/SecurityGuardMainPlugin->getMainPluginClassLoader()Ljava/lang/ClassLoader;":
                return vm.resolveClass("java/lang/ClassLoader").newObject("CL");
            case "com/taobao/wireless/security/adapter/common/SPUtility2->readFromSPUnified(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;":
            case "com/taobao/wireless/security/adapter/common/SPUtility2->read(Ljava/lang/String;)Ljava/lang/String;":
            case "com/taobao/wireless/security/adapter/common/SPUtility2->readSS(Landroid/content/Context;Ljava/lang/String;)Ljava/lang/String;":
                return new StringObject(vm, "");
            case "com/taobao/wireless/security/adapter/datacollection/DeviceInfoCapturer->doCommandForString(I)Ljava/lang/String;":
                return new StringObject(vm, "");
            case "com/taobao/wireless/security/adapter/common/HttpUtil->sendSyncHttpGetRequestBridge(Ljava/lang/String;)Ljava/lang/String;":
            case "com/taobao/wireless/security/adapter/common/HttpUtil->sendSyncHttpPostRequestBridge(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;II)Ljava/lang/String;":
            case "com/taobao/wireless/security/adapter/common/HttpUtil->downloadFileBridge(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;":
                System.out.println("[jni] HttpUtil bridge called (returning empty; no network in harness)");
                return new StringObject(vm, "");
            case "com/taobao/wireless/security/adapter/datareport/DataReportJniBridge->sendReportBridge(Ljava/lang/String;Ljava/lang/String;[B)Ljava/lang/String;":
            case "com/taobao/wireless/security/adapter/datareport/DataReportJniBridge->sendReportBridge(Ljava/lang/String;Ljava/lang/String;Ljava/util/Map;[B)Ljava/lang/String;":
                return new StringObject(vm, "");
            case "com/alibaba/wireless/security/open/edgecomputing/ProxyUtil->getProxyInstance(Ljava/lang/String;Ljava/lang/Object;I)Ljava/lang/Object;":
                return null;
            case "com/alibaba/wireless/security/open/edgecomputing/ProxyUtil->getHandler()Landroid/os/Handler;":
                return vm.resolveClass("android/os/Handler").newObject("H");
        }
        System.out.println("[jni] UNHANDLED callStaticObjectMethodV: " + signature);
        return null;
    }

    @Override
    public boolean callStaticBooleanMethod(BaseVM vm, DvmClass dvmClass, String signature, VarArg vaList) {
        return staticBoolCore(vm, dvmClass, signature, vaList);
    }

    @Override
    public boolean callStaticBooleanMethodV(BaseVM vm, DvmClass dvmClass, String signature, VaList vaList) {
        return staticBoolCore(vm, dvmClass, signature, vaList);
    }

    public boolean staticBoolCore(BaseVM vm, DvmClass dvmClass, String signature, VarArg vaList) {
        switch (signature) {
            case "com/alibaba/wireless/security/framework/ApmMonitorAdapter->isEnableFullTrackRecord()Z":
            case "com/alibaba/wireless/security/framework/ApmMonitorAdapter->isForeground()Z":
                return false;
            case "com/taobao/wireless/security/adapter/common/SPUtility2->saveToFileUnifiedForNative(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Z)I":
                return false;
            case "com/taobao/wireless/security/adapter/common/SPUtility2->removeFromSPUnified(Ljava/lang/String;Ljava/lang/String;Z)Z":
            case "com/taobao/wireless/security/adapter/common/SPUtility2->writeSS(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;)Z":
                return true;
        }
        System.out.println("[jni] UNHANDLED callStaticBooleanMethodV: " + signature);
        return false;
    }

    @Override
    public int callStaticIntMethod(BaseVM vm, DvmClass dvmClass, String signature, VarArg vaList) {
        if (signature.startsWith("com/taobao/wireless/security/adapter/datareport/DataReportJniBridge->")) return 0;
        if (signature.startsWith("com/taobao/wireless/security/adapter/common/SPUtility2->saveToFileUnifiedForNative")) return 0;
        System.out.println("[jni] UNHANDLED callStaticIntMethod: " + signature);
        return 0;
    }

    @Override
    public int callStaticIntMethodV(BaseVM vm, DvmClass dvmClass, String signature, VaList vaList) {
        if (signature.startsWith("com/taobao/wireless/security/adapter/datareport/DataReportJniBridge->")) return 0;
        if (signature.startsWith("com/taobao/wireless/security/adapter/common/SPUtility2->saveToFileUnifiedForNative")) return 0;
        System.out.println("[jni] UNHANDLED callStaticIntMethodV: " + signature);
        return super.callStaticIntMethodV(vm, dvmClass, signature, vaList);
    }

    @Override
    public long callStaticLongMethod(BaseVM vm, DvmClass dvmClass, String signature, VarArg vaList) {
        return staticLongCore(vm, dvmClass, signature, vaList);
    }

    @Override
    public long callStaticLongMethodV(BaseVM vm, DvmClass dvmClass, String signature, VaList vaList) {
        return staticLongCore(vm, dvmClass, signature, vaList);
    }

    public long staticLongCore(BaseVM vm, DvmClass dvmClass, String signature, VarArg vaList) {
        if (signature.equals("android/os/SystemClock->elapsedRealtime()J")) return 123456789L;
        if (signature.equals("android/os/SystemClock->uptimeMillis()J")) return System.currentTimeMillis() % 100000000L;
        System.out.println("[jni] UNHANDLED callStaticLongMethodV: " + signature);
        return 0L;
    }

    @Override
    public void callStaticVoidMethod(BaseVM vm, DvmClass dvmClass, String signature, VarArg vaList) {
        staticVoidCore(vm, dvmClass, signature, vaList);
    }

    @Override
    public void callStaticVoidMethodV(BaseVM vm, DvmClass dvmClass, String signature, VaList vaList) {
        staticVoidCore(vm, dvmClass, signature, vaList);
    }

    public void staticVoidCore(BaseVM vm, DvmClass dvmClass, String signature, VarArg vaList) {
        switch (signature) {
            case "com/taobao/wireless/security/adapter/common/SPUtility2->write(Ljava/lang/String;Ljava/lang/String;)V":
            case "com/alibaba/wireless/security/open/edgecomputing/ECMiscInfo->registerAppLifeCyCleCallBack()V":
                return;
        }
        System.out.println("[jni] UNHANDLED callStaticVoidMethodV: " + signature);
    }

    @Override
    public DvmObject<?> newObject(BaseVM vm, DvmClass dvmClass, String signature, VarArg vaList) {
        return newObjCore(vm, dvmClass, signature, vaList);
    }

    @Override
    public DvmObject<?> newObjectV(BaseVM vm, DvmClass dvmClass, String signature, VaList vaList) {
        return newObjCore(vm, dvmClass, signature, vaList);
    }

    public DvmObject<?> newObjCore(BaseVM vm, DvmClass dvmClass, String signature, VarArg vaList) {
        switch (signature) {
            case "java/util/HashMap-><init>(I)V":
            case "java/util/HashMap-><init>()V":
                return dvmClass.newObject(new HashMap<>());
            case "java/lang/Integer-><init>(I)V":
                return dvmClass.newObject(Integer.valueOf(vaList.getIntArg(0)));
            case "java/lang/Boolean-><init>(Z)V":
                return dvmClass.newObject(Boolean.valueOf(vaList.getIntArg(0) != 0));
            case "java/lang/Long-><init>(J)V":
                return dvmClass.newObject(Long.valueOf(vaList.getLongArg(0)));
            case "java/lang/Float-><init>(F)V":
                return dvmClass.newObject(Float.valueOf(vaList.getFloatArg(0)));
            case "java/lang/Double-><init>(D)V":
                return dvmClass.newObject(Double.valueOf(vaList.getDoubleArg(0)));
            case "com/alibaba/wireless/security/open/SecException-><init>(Ljava/lang/String;I)V": {
                Object msg = vaList.getObjectArg(0) == null ? null : vaList.getObjectArg(0).getValue();
                System.out.println("[jni] SecException constructed by native: msg=" + msg + " code=" + vaList.getIntArg(1));
                return dvmClass.newObject("SecException:" + msg + ":" + vaList.getIntArg(1));
            }
        }
        System.out.println("[jni] UNHANDLED newObjectV: " + signature);
        return dvmClass.newObject(null);
    }

    @Override
    public DvmObject<?> callObjectMethod(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg vaList) {
        return objCore(vm, dvmObject, signature, vaList);
    }

    @Override
    public DvmObject<?> callObjectMethodV(BaseVM vm, DvmObject<?> dvmObject, String signature, VaList vaList) {
        return objCore(vm, dvmObject, signature, vaList);
    }

    public DvmObject<?> objCore(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg vaList) {
        // Integer/Boolean unboxing helpers and Map access are needed by SG native code
        switch (signature) {
            case "java/util/HashMap->keySet()Ljava/util/Set;":
            case "java/util/Map->keySet()Ljava/util/Set;": {
                @SuppressWarnings("unchecked") Map<String, Object> m = (Map<String, Object>) dvmObject.getValue();
                return vm.resolveClass("java/util/Set").newObject(new java.util.HashSet<>(m.keySet()));
            }
            case "java/util/Set->toArray()[Ljava/lang/Object;":
            case "java/util/HashSet->toArray()[Ljava/lang/Object;": {
                @SuppressWarnings("unchecked") java.util.Set<Object> set = (java.util.Set<Object>) dvmObject.getValue();
                DvmObject<?>[] elems = new DvmObject<?>[set.size()];
                int i = 0;
                for (Object e : set) elems[i++] = new StringObject(vm, String.valueOf(e));
                return new ArrayObject(elems);
            }
            case "java/util/HashMap->put(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;":
            case "java/util/Map->put(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;": {
                @SuppressWarnings("unchecked") Map<Object, Object> m = (Map<Object, Object>) dvmObject.getValue();
                Object prev = m.put(vaList.getObjectArg(0).getValue(), vaList.getObjectArg(1).getValue());
                return prev == null ? null : new StringObject(vm, String.valueOf(prev));
            }
            case "java/lang/String->getBytes(Ljava/lang/String;)[B": {
                String s = String.valueOf(dvmObject.getValue());
                Object cs = vaList.getObjectArg(0).getValue();
                try {
                    return new com.github.unidbg.linux.android.dvm.array.ByteArray(vm, s.getBytes(String.valueOf(cs)));
                } catch (Exception e) {
                    return new com.github.unidbg.linux.android.dvm.array.ByteArray(vm, s.getBytes());
                }
            }
            case "java/lang/ClassLoader->loadClass(Ljava/lang/String;)Ljava/lang/Class;": {
                Object name = vaList.getObjectArg(0).getValue();
                System.out.println("[jni] ClassLoader.loadClass(" + name + ")");
                return vm.resolveClass(String.valueOf(name).replace('.', '/'));
            }
        }
        switch (signature) {
            case "java/util/HashMap->get(Ljava/lang/Object;)Ljava/lang/Object;":
            case "java/util/Map->get(Ljava/lang/Object;)Ljava/lang/Object;": {
                @SuppressWarnings("unchecked") Map<String, Object> m = (Map<String, Object>) dvmObject.getValue();
                Object k = vaList.getObjectArg(0).getValue();
                Object v = m == null ? null : m.get(k);
                System.out.println("[jni] Map.get(" + k + ") -> " + describe(v));
                return v == null ? null : new StringObject(vm, String.valueOf(v));
            }
        }
        if (signature.startsWith("android/content/Context->") || signature.startsWith("android/content/ContextWrapper->")) {
            return handleContextCall(signature, vaList);
        }
        if (signature.startsWith("java/io/File->")) {
            return handleFileCall(dvmObject, signature, vaList);
        }
        System.out.println("[jni] UNHANDLED callObjectMethodV: " + signature + " on " + dvmObject);
        System.out.println("[jni] UNHANDLED callObjectMethod: " + signature); return null;
    }

    private DvmObject<?> handleContextCall(String signature, VarArg vaList) {
        switch (signature) {
            case "android/content/Context->getPackageName()Ljava/lang/String;":
            case "android/content/ContextWrapper->getPackageName()Ljava/lang/String;":
                return new StringObject(vm, PKG);
            case "android/content/Context->getPackageCodePath()Ljava/lang/String;":
            case "android/content/ContextWrapper->getPackageCodePath()Ljava/lang/String;":
                return new StringObject(vm, "/data/app/" + PKG + "-1/base.apk");
            case "android/content/Context->getPackageResourcePath()Ljava/lang/String;":
                return new StringObject(vm, "/data/app/" + PKG + "-1/base.apk");
            case "android/content/Context->getApplicationContext()Landroid/content/Context;":
            case "android/content/ContextWrapper->getApplicationContext()Landroid/content/Context;":
            case "android/content/ContextWrapper->getBaseContext()Landroid/content/Context;":
                return ctxObject();
            case "android/content/Context->getFilesDir()Ljava/io/File;":
            case "android/content/ContextWrapper->getFilesDir()Ljava/io/File;":
                return fileObj("/data/data/" + PKG + "/files");
            case "android/content/Context->getCacheDir()Ljava/io/File;":
                return fileObj("/data/data/" + PKG + "/cache");
            case "android/content/Context->getDir(Ljava/lang/String;I)Ljava/io/File;": {
                Object name = vaList.getObjectArg(0).getValue();
                return fileObj("/data/data/" + PKG + "/app_" + name);
            }
            case "android/content/Context->getExternalFilesDir(Ljava/lang/String;)Ljava/io/File;":
                return fileObj("/sdcard/Android/data/" + PKG + "/files");
            case "android/content/Context->getPackageManager()Landroid/content/pm/PackageManager;":
            case "android/content/ContextWrapper->getPackageManager()Landroid/content/pm/PackageManager;":
                return vm.resolveClass("android/content/pm/PackageManager").newObject("PM");
            case "android/content/Context->getApplicationInfo()Landroid/content/pm/ApplicationInfo;":
                return vm.resolveClass("android/content/pm/ApplicationInfo").newObject("APPINFO");
            case "android/content/Context->getContentResolver()Landroid/content/ContentResolver;":
                return vm.resolveClass("android/content/ContentResolver").newObject("CR");
            case "android/content/Context->getSystemService(Ljava/lang/String;)Ljava/lang/Object;": {
                Object name = vaList.getObjectArg(0).getValue();
                System.out.println("[jni] Context.getSystemService(" + name + ") -> null-object");
                return vm.resolveClass("java/lang/Object").newObject(null);
            }
            case "android/content/Context->getSharedPreferences(Ljava/lang/String;I)Landroid/content/SharedPreferences;":
                return vm.resolveClass("android/content/SharedPreferences").newObject(new HashMap<String, Object>());
            case "android/content/Context->getClassLoader()Ljava/lang/ClassLoader;":
                return vm.resolveClass("java/lang/ClassLoader").newObject("CL");
            case "android/content/Context->getResources()Landroid/content/res/Resources;":
                return vm.resolveClass("android/content/res/Resources").newObject("RES");
            case "android/content/Context->getAssets()Landroid/content/res/AssetManager;":
                return vm.resolveClass("android/content/res/AssetManager").newObject("AM");
        }
        System.out.println("[jni] UNHANDLED Context call: " + signature);
        return null;
    }

    private DvmObject<?> fileObj(String path) {
        return vm.resolveClass("java/io/File").newObject(path);
    }

    private DvmObject<?> handleFileCall(DvmObject<?> dvmObject, String signature, VarArg vaList) {
        String path = String.valueOf(dvmObject.getValue());
        switch (signature) {
            case "java/io/File->getAbsolutePath()Ljava/lang/String;":
            case "java/io/File->getPath()Ljava/lang/String;":
                return new StringObject(vm, path);
            case "java/io/File->getName()Ljava/lang/String;":
                return new StringObject(vm, new File(path).getName());
            case "java/io/File->getParent()Ljava/lang/String;":
                return new StringObject(vm, String.valueOf(new File(path).getParent()));
        }
        System.out.println("[jni] UNHANDLED File call: " + signature);
        return null;
    }

    @Override
    public int callIntMethod(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg vaList) {
        return intCore(vm, dvmObject, signature, vaList);
    }

    @Override
    public int callIntMethodV(BaseVM vm, DvmObject<?> dvmObject, String signature, VaList vaList) {
        return intCore(vm, dvmObject, signature, vaList);
    }

    public int intCore(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg vaList) {
        if (signature.equals("java/lang/Integer->intValue()I") && dvmObject.getValue() instanceof Number) {
            return ((Number) dvmObject.getValue()).intValue();
        }
        if ((signature.equals("java/util/HashMap->size()I") || signature.equals("java/util/Map->size()I"))
                && dvmObject.getValue() instanceof Map) {
            return ((Map<?, ?>) dvmObject.getValue()).size();
        }
        if (signature.startsWith("android/content/SharedPreferences->getInt")) {
            return 0;
        }
        if (signature.equals("android/content/Context->checkSelfPermission(Ljava/lang/String;)I")
                || signature.equals("android/content/Context->checkCallingOrSelfPermission(Ljava/lang/String;)I")) {
            return 0; // PERMISSION_GRANTED
        }
        System.out.println("[jni] UNHANDLED callIntMethodV: " + signature + " on " + dvmObject);
        System.out.println("[jni] UNHANDLED callIntMethod: " + signature); return 0;
    }

    @Override
    public boolean callBooleanMethod(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg vaList) {
        return boolCore(vm, dvmObject, signature, vaList);
    }

    @Override
    public boolean callBooleanMethodV(BaseVM vm, DvmObject<?> dvmObject, String signature, VaList vaList) {
        return boolCore(vm, dvmObject, signature, vaList);
    }

    public boolean boolCore(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg vaList) {
        if (signature.equals("java/lang/Boolean->booleanValue()Z") && dvmObject.getValue() instanceof Boolean) {
            return (Boolean) dvmObject.getValue();
        }
        if (signature.equals("java/io/File->exists()Z") || signature.equals("java/io/File->mkdirs()Z")
                || signature.equals("java/io/File->mkdir()Z") || signature.equals("java/io/File->canRead()Z")
                || signature.equals("java/io/File->canWrite()Z")) {
            return true;
        }
        System.out.println("[jni] UNHANDLED callBooleanMethodV: " + signature + " on " + dvmObject);
        System.out.println("[jni] UNHANDLED callBooleanMethod: " + signature); return false;
    }

    @Override
    public long callLongMethod(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg vaList) {
        return longCore(vm, dvmObject, signature, vaList);
    }

    @Override
    public long callLongMethodV(BaseVM vm, DvmObject<?> dvmObject, String signature, VaList vaList) {
        return longCore(vm, dvmObject, signature, vaList);
    }

    public long longCore(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg vaList) {
        if (signature.equals("java/lang/Long->longValue()J") && dvmObject.getValue() instanceof Number) {
            return ((Number) dvmObject.getValue()).longValue();
        }
        if (signature.equals("java/io/File->length()J")) return 0L;
        if (signature.equals("java/io/File->lastModified()J")) return System.currentTimeMillis();
        System.out.println("[jni] UNHANDLED callLongMethodV: " + signature + " on " + dvmObject);
        System.out.println("[jni] UNHANDLED callLongMethod: " + signature); return 0L;
    }


    @Override
    public void callVoidMethod(BaseVM vm, DvmObject<?> dvmObject, String signature, VarArg vaList) {
        System.out.println("[jni] void method (no-op): " + signature);
    }

    @Override
    public void callVoidMethodV(BaseVM vm, DvmObject<?> dvmObject, String signature, VaList vaList) {
        System.out.println("[jni] void method V (no-op): " + signature);
    }

    @Override
    public DvmObject<?> getObjectField(BaseVM vm, DvmObject<?> dvmObject, String signature) {
        switch (signature) {
            case "android/content/pm/ApplicationInfo->sourceDir:Ljava/lang/String;":
            case "android/content/pm/ApplicationInfo->publicSourceDir:Ljava/lang/String;":
                return new StringObject(vm, "/data/app/" + PKG + "-1/base.apk");
            case "android/content/pm/ApplicationInfo->nativeLibraryDir:Ljava/lang/String;":
                return new StringObject(vm, "/data/app/" + PKG + "-1/lib/arm64");
            case "android/content/pm/ApplicationInfo->dataDir:Ljava/lang/String;":
                return new StringObject(vm, "/data/data/" + PKG);
            case "android/content/pm/PackageInfo->versionName:Ljava/lang/String;":
                return new StringObject(vm, "7.28.70");
        }
        System.out.println("[jni] UNHANDLED getObjectField: " + signature + " on " + dvmObject);
        return super.getObjectField(vm, dvmObject, signature);
    }

    @Override
    public int getIntField(BaseVM vm, DvmObject<?> dvmObject, String signature) {
        switch (signature) {
            case "android/content/pm/ApplicationInfo->uid:I":
                return 10234;
            case "android/content/pm/PackageInfo->versionCode:I":
                return 469;
        }
        System.out.println("[jni] UNHANDLED getIntField: " + signature + " on " + dvmObject);
        return super.getIntField(vm, dvmObject, signature);
    }

    @Override
    public DvmObject<?> getStaticObjectField(BaseVM vm, DvmClass dvmClass, String signature) {
        if (signature.startsWith("android/os/Build->")) {
            String v;
            switch (signature) {
                case "android/os/Build->BRAND:Ljava/lang/String;": v = "Xiaomi"; break;
                case "android/os/Build->MODEL:Ljava/lang/String;": v = "MI 9"; break;
                case "android/os/Build->MANUFACTURER:Ljava/lang/String;": v = "Xiaomi"; break;
                case "android/os/Build->PRODUCT:Ljava/lang/String;": v = "cepheus"; break;
                case "android/os/Build->DEVICE:Ljava/lang/String;": v = "cepheus"; break;
                case "android/os/Build->BOARD:Ljava/lang/String;": v = "msmnile"; break;
                case "android/os/Build->HARDWARE:Ljava/lang/String;": v = "qcom"; break;
                case "android/os/Build->FINGERPRINT:Ljava/lang/String;": v = "Xiaomi/cepheus/cepheus:10/QKQ1.190825.002/V12.0.4.0.QFACNXM:user/release-keys"; break;
                case "android/os/Build->ID:Ljava/lang/String;": v = "QKQ1.190825.002"; break;
                case "android/os/Build->DISPLAY:Ljava/lang/String;": v = "QKQ1.190825.002"; break;
                case "android/os/Build->HOST:Ljava/lang/String;": v = "builder"; break;
                case "android/os/Build->USER:Ljava/lang/String;": v = "builder"; break;
                case "android/os/Build->TAGS:Ljava/lang/String;": v = "release-keys"; break;
                case "android/os/Build->TYPE:Ljava/lang/String;": v = "user"; break;
                case "android/os/Build->SERIAL:Ljava/lang/String;": v = "unknown"; break;
                case "android/os/Build$VERSION->RELEASE:Ljava/lang/String;": v = "10"; break;
                case "android/os/Build$VERSION->CODENAME:Ljava/lang/String;": v = "REL"; break;
                case "android/os/Build$VERSION->INCREMENTAL:Ljava/lang/String;": v = "V12.0.4.0.QFACNXM"; break;
                case "android/os/Build$VERSION->SECURITY_PATCH:Ljava/lang/String;": v = "2020-09-01"; break;
                default:
                    System.out.println("[jni] UNHANDLED Build field: " + signature);
                    return new StringObject(vm, "");
            }
            return new StringObject(vm, v);
        }
        System.out.println("[jni] UNHANDLED getStaticObjectField: " + signature);
        return super.getStaticObjectField(vm, dvmClass, signature);
    }

    @Override
    public int getStaticIntField(BaseVM vm, DvmClass dvmClass, String signature) {
        if (signature.equals("android/os/Build$VERSION->SDK_INT:I")) return 29;
        System.out.println("[jni] UNHANDLED getStaticIntField: " + signature);
        return super.getStaticIntField(vm, dvmClass, signature);
    }
}
