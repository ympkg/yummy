// YmCompilerWorker — Long-running javac worker for ym build tool.
// Reads compile requests (JSON) from stdin, compiles via javax.tools API, writes JSON responses to stdout.
// This eliminates JVM startup overhead for per-module compilation in workspace builds.

import javax.tools.*;
import java.io.*;
import java.nio.charset.*;
import java.nio.file.*;
import java.util.*;
import java.util.stream.*;

public class YmCompilerWorker {
    private static JavaCompiler compiler;

    public static void main(String[] args) throws Exception {
        compiler = ToolProvider.getSystemJavaCompiler();
        if (compiler == null) {
            System.out.println("{\"success\":false,\"filesCompiled\":0,\"diagnostics\":" +
                jsonEscape("No system Java compiler found (ToolProvider.getSystemJavaCompiler() returned null). Ensure you are running a JDK, not a JRE.") + "}");
            System.out.flush();
            return;
        }

        BufferedReader reader = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8));
        String line;
        while ((line = reader.readLine()) != null) {
            line = line.trim();
            if (line.isEmpty()) continue;
            if ("PING".equals(line)) {
                System.out.println("PONG");
                System.out.flush();
                continue;
            }
            handleRequest(line);
        }
    }

    private static void handleRequest(String line) {
        try {
            Map<String, Object> req = parseJson(line);
            String outputDir = str(req, "outputDir");
            String release = str(req, "release");
            String encoding = str(req, "encoding");
            String classpath = str(req, "classpath");
            String processorPath = str(req, "processorPath");
            @SuppressWarnings("unchecked")
            List<String> extraArgs = req.get("args") instanceof List ? (List<String>) req.get("args") : Collections.emptyList();
            @SuppressWarnings("unchecked")
            List<String> files = req.get("files") instanceof List ? (List<String>) req.get("files") : Collections.emptyList();

            if (files.isEmpty()) {
                System.out.println("{\"success\":true,\"filesCompiled\":0,\"diagnostics\":\"\"}");
                System.out.flush();
                return;
            }

            // Ensure output directory exists
            Files.createDirectories(Paths.get(outputDir));

            Charset charset = (encoding != null && !encoding.isEmpty()) ? Charset.forName(encoding) : null;
            DiagnosticCollector<JavaFileObject> diagnostics = new DiagnosticCollector<>();
            StandardJavaFileManager fm = compiler.getStandardFileManager(diagnostics, null, charset);

            // Set locations via file manager API (more reliable than -d/-cp options)
            fm.setLocation(StandardLocation.CLASS_OUTPUT, Collections.singletonList(new File(outputDir)));

            if (classpath != null && !classpath.isEmpty()) {
                List<File> cpFiles = Arrays.stream(classpath.split(File.pathSeparator))
                    .map(File::new).collect(Collectors.toList());
                fm.setLocation(StandardLocation.CLASS_PATH, cpFiles);
            }

            if (processorPath != null && !processorPath.isEmpty()) {
                List<File> apFiles = Arrays.stream(processorPath.split(File.pathSeparator))
                    .map(File::new).collect(Collectors.toList());
                fm.setLocation(StandardLocation.ANNOTATION_PROCESSOR_PATH, apFiles);
            }

            // Build compiler options (only flags, not paths)
            List<String> options = new ArrayList<>();
            if (release != null && !release.isEmpty()) {
                options.add("--release");
                options.add(release);
            }
            if (encoding != null && !encoding.isEmpty()) {
                options.add("-encoding");
                options.add(encoding);
            }
            options.addAll(extraArgs);

            // Compile
            List<File> sourceFiles = files.stream().map(File::new).collect(Collectors.toList());
            Iterable<? extends JavaFileObject> units = fm.getJavaFileObjectsFromFiles(sourceFiles);
            StringWriter sw = new StringWriter();
            JavaCompiler.CompilationTask task = compiler.getTask(sw, fm, diagnostics, options, null, units);
            boolean success = task.call();
            fm.close();

            // Collect diagnostics
            StringBuilder diagStr = new StringBuilder();
            for (Diagnostic<? extends JavaFileObject> d : diagnostics.getDiagnostics()) {
                if (d.getSource() != null) {
                    diagStr.append(d.getSource().getName());
                    if (d.getLineNumber() != Diagnostic.NOPOS) {
                        diagStr.append(':').append(d.getLineNumber());
                    }
                    diagStr.append(": ");
                }
                diagStr.append(d.getKind().toString().toLowerCase()).append(": ");
                diagStr.append(d.getMessage(null)).append('\n');
            }
            String directOutput = sw.toString();
            if (!directOutput.isEmpty()) {
                diagStr.append(directOutput);
            }

            System.out.println("{\"success\":" + success + ",\"filesCompiled\":" + files.size() +
                ",\"diagnostics\":" + jsonEscape(diagStr.toString()) + "}");
            System.out.flush();

        } catch (Exception e) {
            StringWriter ew = new StringWriter();
            e.printStackTrace(new PrintWriter(ew));
            System.out.println("{\"success\":false,\"filesCompiled\":0,\"diagnostics\":" +
                jsonEscape(ew.toString()) + "}");
            System.out.flush();
        }
    }

    // --- JSON helpers ---

    static String jsonEscape(String s) {
        if (s == null) return "\"\"";
        StringBuilder sb = new StringBuilder(s.length() + 2);
        sb.append('"');
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"': sb.append("\\\""); break;
                case '\\': sb.append("\\\\"); break;
                case '\n': sb.append("\\n"); break;
                case '\r': sb.append("\\r"); break;
                case '\t': sb.append("\\t"); break;
                default: sb.append(c);
            }
        }
        sb.append('"');
        return sb.toString();
    }

    static String str(Map<String, Object> map, String key) {
        Object v = map.get(key);
        return v instanceof String ? (String) v : null;
    }

    // Minimal JSON parser — handles flat objects with string values, string arrays, booleans, null
    static Map<String, Object> parseJson(String json) {
        Map<String, Object> map = new LinkedHashMap<>();
        int i = json.indexOf('{');
        if (i < 0) return map;
        i++;
        int len = json.length();
        while (i < len) {
            i = skipWs(json, i, len);
            if (i >= len || json.charAt(i) == '}') break;
            if (json.charAt(i) == ',') { i++; continue; }
            if (json.charAt(i) != '"') { i++; continue; }

            int[] kr = readStr(json, i, len);
            String key = unescape(json.substring(kr[0], kr[1]));
            i = kr[2];
            i = skipWs(json, i, len);
            if (i < len && json.charAt(i) == ':') i++;
            i = skipWs(json, i, len);
            if (i >= len) break;

            char c = json.charAt(i);
            if (c == '"') {
                int[] vr = readStr(json, i, len);
                map.put(key, unescape(json.substring(vr[0], vr[1])));
                i = vr[2];
            } else if (c == '[') {
                List<String> list = new ArrayList<>();
                i++;
                while (i < len && json.charAt(i) != ']') {
                    i = skipWs(json, i, len);
                    if (i >= len) break;
                    if (json.charAt(i) == ',') { i++; continue; }
                    if (json.charAt(i) == ']') break;
                    if (json.charAt(i) == '"') {
                        int[] vr = readStr(json, i, len);
                        list.add(unescape(json.substring(vr[0], vr[1])));
                        i = vr[2];
                    } else { i++; }
                }
                if (i < len) i++;
                map.put(key, list);
            } else if (json.startsWith("true", i)) {
                map.put(key, Boolean.TRUE); i += 4;
            } else if (json.startsWith("false", i)) {
                map.put(key, Boolean.FALSE); i += 5;
            } else if (json.startsWith("null", i)) {
                map.put(key, null); i += 4;
            } else { i++; }
        }
        return map;
    }

    static int skipWs(String s, int i, int len) {
        while (i < len && s.charAt(i) <= ' ') i++;
        return i;
    }

    // Returns [contentStart, contentEnd, nextPosition]
    static int[] readStr(String s, int i, int len) {
        i++; // skip opening quote
        int start = i;
        while (i < len) {
            char c = s.charAt(i);
            if (c == '\\') { i += 2; continue; }
            if (c == '"') break;
            i++;
        }
        int end = i;
        if (i < len) i++;
        return new int[]{start, end, i};
    }

    static String unescape(String s) {
        if (s.indexOf('\\') < 0) return s;
        StringBuilder sb = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == '\\' && i + 1 < s.length()) {
                switch (s.charAt(++i)) {
                    case '"': sb.append('"'); break;
                    case '\\': sb.append('\\'); break;
                    case 'n': sb.append('\n'); break;
                    case 'r': sb.append('\r'); break;
                    case 't': sb.append('\t'); break;
                    default: sb.append('\\'); sb.append(s.charAt(i));
                }
            } else {
                sb.append(c);
            }
        }
        return sb.toString();
    }
}
