package dev.yummy.agent;

import java.io.*;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Yummy Dev Agent - Hot reload agent for ym dev mode.
 *
 * Three-layer hot reload strategy:
 *   L1: JDWP HotSwap - method body changes only (<100ms)
 *   L2: ClassLoader reload - structural changes (new methods/fields) (<500ms)
 *   L3: Fast restart - when ClassLoader reload fails (1-3s)
 *
 * The agent is injected into the target JVM via -javaagent flag.
 * It listens for reload commands from the ym Rust process via TCP.
 *
 * Protocol (line-delimited JSON):
 *   Request:  {"method":"reload","params":{"classes":["com.example.Foo"],"classDir":"out/classes"}}
 *   Response: {"success":true,"strategy":"hotswap","timeMs":45}
 */
public class YummyDevAgent {

    private static Instrumentation instrumentation;
    private static int port;
    private static volatile boolean running = true;

    // Track loaded classes for reload
    private static final Map<String, byte[]> classCache = new ConcurrentHashMap<>();

    // User code ClassLoader (for L2 reload)
    private static volatile YummyClassLoader userClassLoader;
    private static String[] userClassPath;

    /**
     * Called when agent is loaded at JVM startup (-javaagent:ym-agent.jar=port=9876)
     */
    public static void premain(String agentArgs, Instrumentation inst) {
        instrumentation = inst;
        parseArgs(agentArgs);

        // Start reload listener in background
        Thread listenerThread = new Thread(YummyDevAgent::startListener, "ym-agent-listener");
        listenerThread.setDaemon(true);
        listenerThread.start();

        System.err.println("[ym-agent] Hot reload agent active on port " + port);
    }

    /**
     * Called when agent is dynamically attached
     */
    public static void agentmain(String agentArgs, Instrumentation inst) {
        premain(agentArgs, inst);
    }

    private static void parseArgs(String agentArgs) {
        if (agentArgs != null) {
            for (String part : agentArgs.split(",")) {
                String[] kv = part.split("=", 2);
                if (kv.length == 2 && "port".equals(kv[0])) {
                    port = Integer.parseInt(kv[1]);
                }
            }
        }
        if (port == 0) {
            port = 9876; // default
        }
    }

    private static void startListener() {
        try (ServerSocket server = new ServerSocket(port)) {
            while (running) {
                try {
                    Socket socket = server.accept();
                    handleReloadRequest(socket);
                } catch (IOException e) {
                    if (running) {
                        System.err.println("[ym-agent] Error: " + e.getMessage());
                    }
                }
            }
        } catch (IOException e) {
            System.err.println("[ym-agent] Failed to start listener: " + e.getMessage());
        }
    }

    private static void handleReloadRequest(Socket socket) {
        try (BufferedReader reader = new BufferedReader(new InputStreamReader(socket.getInputStream()));
             PrintWriter writer = new PrintWriter(new OutputStreamWriter(socket.getOutputStream()), true)) {

            String line = reader.readLine();
            if (line == null) return;

            // Parse request
            // Simple parsing: extract classDir and class names
            String classDir = extractJsonString(line, "classDir");
            List<String> classNames = extractJsonArray(line, "classes");
            String method = extractJsonString(line, "method");

            if ("shutdown".equals(method)) {
                running = false;
                writer.println("{\"success\":true}");
                return;
            }

            if ("reload".equals(method) && classDir != null && !classNames.isEmpty()) {
                ReloadResult result = reload(classDir, classNames);
                writer.println(String.format(
                    "{\"success\":%s,\"strategy\":\"%s\",\"timeMs\":%d,\"error\":%s}",
                    result.success, result.strategy, result.timeMs,
                    result.error == null ? "null" : "\"" + result.error.replace("\"", "\\\"") + "\""
                ));
            } else {
                writer.println("{\"success\":false,\"error\":\"invalid request\"}");
            }

        } catch (Exception e) {
            System.err.println("[ym-agent] Request error: " + e.getMessage());
        }
    }

    /**
     * Attempt to reload classes using the three-layer strategy.
     */
    private static ReloadResult reload(String classDir, List<String> classNames) {
        long start = System.nanoTime();
        String l1Error = null;
        String l2Error = null;

        // L1: Try HotSwap (redefine classes in-place)
        try {
            if (tryHotSwap(classDir, classNames)) {
                long elapsed = (System.nanoTime() - start) / 1_000_000;
                return new ReloadResult(true, "hotswap", elapsed, null);
            }
            l1Error = "class not loaded yet";
        } catch (Exception e) {
            l1Error = e.getMessage();
            System.err.println("[ym-agent] L1 HotSwap failed: " + l1Error);
        }

        // L2: Try ClassLoader reload
        try {
            if (tryClassLoaderReload(classDir, classNames)) {
                long elapsed = (System.nanoTime() - start) / 1_000_000;
                return new ReloadResult(true, "classloader", elapsed, null);
            }
            l2Error = "class not found";
        } catch (Exception e) {
            l2Error = e.getMessage();
            System.err.println("[ym-agent] L2 ClassLoader reload failed: " + l2Error);
        }

        // L3: Signal that a restart is needed
        long elapsed = (System.nanoTime() - start) / 1_000_000;
        String reason = "L1: " + l1Error + ", L2: " + l2Error;
        return new ReloadResult(false, "restart", elapsed, reason);
    }

    /**
     * L1: HotSwap - redefine method bodies using Instrumentation API.
     * Only works for method body changes (no structural changes).
     */
    private static boolean tryHotSwap(String classDir, List<String> classNames) throws Exception {
        if (!instrumentation.isRedefineClassesSupported()) {
            return false;
        }

        List<ClassDefinition> definitions = new ArrayList<>();

        for (String className : classNames) {
            // Find the loaded class
            Class<?> loadedClass = findLoadedClass(className);
            if (loadedClass == null) {
                return false; // New class, can't hotswap
            }

            // Read new class bytes
            String classFile = className.replace('.', '/') + ".class";
            Path classPath = Path.of(classDir, classFile);
            if (!Files.exists(classPath)) {
                return false;
            }

            byte[] newBytes = Files.readAllBytes(classPath);
            definitions.add(new ClassDefinition(loadedClass, newBytes));
        }

        // Attempt redefine - will throw if structural changes detected
        instrumentation.redefineClasses(definitions.toArray(new ClassDefinition[0]));
        return true;
    }

    /**
     * L2: ClassLoader reload - create a new ClassLoader for user code.
     * Works for structural changes (new methods, fields, classes).
     */
    private static boolean tryClassLoaderReload(String classDir, List<String> classNames) throws Exception {
        // Create new ClassLoader with updated classes
        YummyClassLoader newLoader = new YummyClassLoader(
            new java.net.URL[]{Path.of(classDir).toUri().toURL()},
            YummyDevAgent.class.getClassLoader()
        );

        // Verify all classes can be loaded
        for (String className : classNames) {
            try {
                newLoader.loadClass(className);
            } catch (ClassNotFoundException e) {
                return false;
            }
        }

        // Replace the old ClassLoader
        YummyClassLoader oldLoader = userClassLoader;
        userClassLoader = newLoader;

        // The old ClassLoader will be GC'd
        if (oldLoader != null) {
            oldLoader.close();
        }

        return true;
    }

    private static Class<?> findLoadedClass(String className) {
        for (Class<?> c : instrumentation.getAllLoadedClasses()) {
            if (c.getName().equals(className)) {
                return c;
            }
        }
        return null;
    }

    // Simple JSON helpers (no dependencies)
    private static String extractJsonString(String json, String key) {
        String search = "\"" + key + "\":\"";
        int idx = json.indexOf(search);
        if (idx < 0) return null;
        idx += search.length();
        int end = json.indexOf("\"", idx);
        if (end < 0) return null;
        return json.substring(idx, end);
    }

    private static List<String> extractJsonArray(String json, String key) {
        List<String> result = new ArrayList<>();
        String search = "\"" + key + "\":[";
        int idx = json.indexOf(search);
        if (idx < 0) return result;
        idx += search.length();
        int end = json.indexOf("]", idx);
        if (end < 0) return result;

        String arrayContent = json.substring(idx, end);
        for (String item : arrayContent.split(",")) {
            item = item.trim();
            if (item.startsWith("\"") && item.endsWith("\"")) {
                result.add(item.substring(1, item.length() - 1));
            }
        }
        return result;
    }

    static class ReloadResult {
        final boolean success;
        final String strategy;
        final long timeMs;
        final String error;

        ReloadResult(boolean success, String strategy, long timeMs, String error) {
            this.success = success;
            this.strategy = strategy;
            this.timeMs = timeMs;
            this.error = error;
        }
    }
}
