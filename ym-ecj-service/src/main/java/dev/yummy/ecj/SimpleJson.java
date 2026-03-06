package dev.yummy.ecj;

import java.util.*;

/**
 * Minimal JSON parser to avoid external dependencies.
 * Only handles the subset needed for the compile service protocol.
 */
public class SimpleJson {

    private final String input;
    private int pos;

    private SimpleJson(String input) {
        this.input = input;
        this.pos = 0;
    }

    @SuppressWarnings("unchecked")
    public static Map<String, Object> parse(String json) {
        return (Map<String, Object>) new SimpleJson(json.trim()).parseValue();
    }

    public static String escape(String s) {
        return "\"" + s.replace("\\", "\\\\")
                       .replace("\"", "\\\"")
                       .replace("\n", "\\n")
                       .replace("\r", "\\r")
                       .replace("\t", "\\t") + "\"";
    }

    private Object parseValue() {
        skipWhitespace();
        if (pos >= input.length()) return null;

        char c = input.charAt(pos);
        return switch (c) {
            case '{' -> parseObject();
            case '[' -> parseArray();
            case '"' -> parseString();
            case 't', 'f' -> parseBoolean();
            case 'n' -> parseNull();
            default -> parseNumber();
        };
    }

    private Map<String, Object> parseObject() {
        Map<String, Object> map = new LinkedHashMap<>();
        pos++; // skip {
        skipWhitespace();

        if (pos < input.length() && input.charAt(pos) == '}') {
            pos++;
            return map;
        }

        while (pos < input.length()) {
            skipWhitespace();
            String key = parseString();
            skipWhitespace();
            expect(':');
            Object value = parseValue();
            map.put(key, value);
            skipWhitespace();
            if (pos < input.length() && input.charAt(pos) == ',') {
                pos++;
            } else {
                break;
            }
        }

        skipWhitespace();
        if (pos < input.length() && input.charAt(pos) == '}') pos++;
        return map;
    }

    private List<Object> parseArray() {
        List<Object> list = new ArrayList<>();
        pos++; // skip [
        skipWhitespace();

        if (pos < input.length() && input.charAt(pos) == ']') {
            pos++;
            return list;
        }

        while (pos < input.length()) {
            list.add(parseValue());
            skipWhitespace();
            if (pos < input.length() && input.charAt(pos) == ',') {
                pos++;
            } else {
                break;
            }
        }

        skipWhitespace();
        if (pos < input.length() && input.charAt(pos) == ']') pos++;
        return list;
    }

    private String parseString() {
        expect('"');
        StringBuilder sb = new StringBuilder();
        while (pos < input.length()) {
            char c = input.charAt(pos);
            if (c == '"') {
                pos++;
                return sb.toString();
            }
            if (c == '\\') {
                pos++;
                if (pos < input.length()) {
                    char escaped = input.charAt(pos);
                    switch (escaped) {
                        case '"', '\\', '/' -> sb.append(escaped);
                        case 'n' -> sb.append('\n');
                        case 'r' -> sb.append('\r');
                        case 't' -> sb.append('\t');
                        default -> { sb.append('\\'); sb.append(escaped); }
                    }
                }
            } else {
                sb.append(c);
            }
            pos++;
        }
        return sb.toString();
    }

    private Number parseNumber() {
        int start = pos;
        if (pos < input.length() && input.charAt(pos) == '-') pos++;
        while (pos < input.length() && Character.isDigit(input.charAt(pos))) pos++;

        boolean isFloat = false;
        if (pos < input.length() && input.charAt(pos) == '.') {
            isFloat = true;
            pos++;
            while (pos < input.length() && Character.isDigit(input.charAt(pos))) pos++;
        }

        String numStr = input.substring(start, pos);
        if (isFloat) {
            return Double.parseDouble(numStr);
        } else {
            long val = Long.parseLong(numStr);
            if (val >= Integer.MIN_VALUE && val <= Integer.MAX_VALUE) {
                return (int) val;
            }
            return val;
        }
    }

    private Boolean parseBoolean() {
        if (input.startsWith("true", pos)) {
            pos += 4;
            return true;
        } else if (input.startsWith("false", pos)) {
            pos += 5;
            return false;
        }
        throw new RuntimeException("Expected boolean at position " + pos);
    }

    private Object parseNull() {
        if (input.startsWith("null", pos)) {
            pos += 4;
            return null;
        }
        throw new RuntimeException("Expected null at position " + pos);
    }

    private void skipWhitespace() {
        while (pos < input.length() && Character.isWhitespace(input.charAt(pos))) {
            pos++;
        }
    }

    private void expect(char c) {
        if (pos < input.length() && input.charAt(pos) == c) {
            pos++;
        }
    }
}
