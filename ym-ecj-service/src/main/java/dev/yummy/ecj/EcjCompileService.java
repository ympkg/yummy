package dev.yummy.ecj;

import java.io.*;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import java.util.stream.Collectors;

/**
 * ECJ (Eclipse Compiler for Java) Compile Service.
 *
 * Runs as a long-lived JVM process, accepting compile requests via TCP socket
 * using a JSON-based protocol. This avoids JVM startup overhead on each compile.
 *
 * Protocol (line-delimited JSON):
 *   Request:  {"method":"compile","id":1,"params":{...}}
 *   Response: {"success":true,"compiledFiles":3,"errors":[],"warnings":[],"timeMs":87}
 *   Shutdown: {"method":"shutdown","id":0,"params":{}}
 */
public class EcjCompileService {

    private final int port;
    private volatile boolean running = true;

    // Cache for dependency tracking across compilations
    private final Map<String, Set<String>> dependencyMap = new ConcurrentHashMap<>();

    public EcjCompileService(int port) {
        this.port = port;
    }

    public void start() throws IOException {
        try (ServerSocket serverSocket = new ServerSocket(port)) {
            System.err.println("[ecj-service] Listening on port " + port);

            while (running) {
                try {
                    Socket socket = serverSocket.accept();
                    handleConnection(socket);
                } catch (IOException e) {
                    if (running) {
                        System.err.println("[ecj-service] Connection error: " + e.getMessage());
                    }
                }
            }
        }
    }

    private void handleConnection(Socket socket) {
        try (BufferedReader reader = new BufferedReader(new InputStreamReader(socket.getInputStream()));
             PrintWriter writer = new PrintWriter(new OutputStreamWriter(socket.getOutputStream()), true)) {

            String line = reader.readLine();
            if (line == null) return;

            // Simple JSON parsing (avoiding external dependencies)
            Map<String, Object> request = SimpleJson.parse(line);
            String method = (String) request.get("method");

            if ("shutdown".equals(method)) {
                running = false;
                writer.println("{\"success\":true}");
                return;
            }

            if ("compile".equals(method)) {
                @SuppressWarnings("unchecked")
                Map<String, Object> params = (Map<String, Object>) request.get("params");
                String response = handleCompile(params);
                writer.println(response);
            }

        } catch (Exception e) {
            System.err.println("[ecj-service] Error: " + e.getMessage());
        }
    }

    private String handleCompile(Map<String, Object> params) {
        long startTime = System.currentTimeMillis();

        @SuppressWarnings("unchecked")
        List<String> changedFiles = (List<String>) params.getOrDefault("changedFiles", List.of());
        @SuppressWarnings("unchecked")
        List<String> sourceDirs = (List<String>) params.getOrDefault("sourceDirs", List.of());
        @SuppressWarnings("unchecked")
        List<String> classpath = (List<String>) params.getOrDefault("classpath", List.of());
        String outputDir = (String) params.getOrDefault("outputDir", "out/classes");
        String sourceVersion = (String) params.get("sourceVersion");
        String encoding = (String) params.get("encoding");

        // Collect files to compile
        List<String> filesToCompile;
        if (changedFiles.isEmpty()) {
            // Full compile: collect all .java files from source dirs
            filesToCompile = new ArrayList<>();
            for (String srcDir : sourceDirs) {
                try {
                    Files.walk(Path.of(srcDir))
                         .filter(p -> p.toString().endsWith(".java"))
                         .forEach(p -> filesToCompile.add(p.toString()));
                } catch (IOException e) {
                    // skip
                }
            }
        } else {
            filesToCompile = new ArrayList<>(changedFiles);
        }

        if (filesToCompile.isEmpty()) {
            return formatResponse(true, 0, List.of(), List.of(), System.currentTimeMillis() - startTime);
        }

        // Build ECJ arguments
        List<String> args = new ArrayList<>();

        // Output directory
        args.add("-d");
        args.add(outputDir);

        // Source version
        if (sourceVersion != null) {
            args.add("--release");
            args.add(sourceVersion);
        }

        // Encoding
        if (encoding != null) {
            args.add("-encoding");
            args.add(encoding);
        }

        // Classpath (include output dir for incremental)
        List<String> fullClasspath = new ArrayList<>(classpath);
        fullClasspath.add(outputDir);
        if (!fullClasspath.isEmpty()) {
            args.add("-classpath");
            args.add(String.join(File.pathSeparator, fullClasspath));
        }

        // Warn but don't fail on some issues
        args.add("-warn:none");
        args.add("-proceedOnError");

        // Source files
        args.addAll(filesToCompile);

        // Create output directory
        try {
            Files.createDirectories(Path.of(outputDir));
        } catch (IOException e) {
            return formatResponse(false, 0, List.of(e.getMessage()), List.of(), 0);
        }

        // Invoke ECJ compiler
        StringWriter errorWriter = new StringWriter();
        StringWriter warningWriter = new StringWriter();

        boolean success;
        try {
            // Use ECJ's BatchCompiler
            success = org.eclipse.jdt.core.compiler.batch.BatchCompiler.compile(
                args.toArray(new String[0]),
                new PrintWriter(warningWriter),
                new PrintWriter(errorWriter),
                null  // CompilationProgress
            );
        } catch (Exception e) {
            return formatResponse(false, 0, List.of("ECJ error: " + e.getMessage()), List.of(),
                    System.currentTimeMillis() - startTime);
        }

        long elapsed = System.currentTimeMillis() - startTime;

        List<String> errors = errorWriter.toString().isEmpty() ?
            List.of() : List.of(errorWriter.toString());
        List<String> warnings = warningWriter.toString().isEmpty() ?
            List.of() : List.of(warningWriter.toString());

        return formatResponse(success, filesToCompile.size(), errors, warnings, elapsed);
    }

    private String formatResponse(boolean success, int compiledFiles,
                                   List<String> errors, List<String> warnings, long timeMs) {
        StringBuilder sb = new StringBuilder();
        sb.append("{\"success\":").append(success);
        sb.append(",\"compiledFiles\":").append(compiledFiles);
        sb.append(",\"errors\":[");
        sb.append(errors.stream().map(SimpleJson::escape).collect(Collectors.joining(",")));
        sb.append("],\"warnings\":[");
        sb.append(warnings.stream().map(SimpleJson::escape).collect(Collectors.joining(",")));
        sb.append("],\"timeMs\":").append(timeMs);
        sb.append("}");
        return sb.toString();
    }

    public static void main(String[] args) {
        int port = 0;
        for (int i = 0; i < args.length; i++) {
            if ("--port".equals(args[i]) && i + 1 < args.length) {
                port = Integer.parseInt(args[i + 1]);
            }
        }

        if (port == 0) {
            System.err.println("Usage: java -jar ym-ecj-service.jar --port <port>");
            System.exit(1);
        }

        try {
            new EcjCompileService(port).start();
        } catch (IOException e) {
            System.err.println("Failed to start: " + e.getMessage());
            System.exit(1);
        }
    }
}
