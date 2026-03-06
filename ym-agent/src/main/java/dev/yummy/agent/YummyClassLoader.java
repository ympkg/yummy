package dev.yummy.agent;

import java.io.IOException;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Dual ClassLoader architecture for hot reload:
 *
 * - System ClassLoader (parent): loads framework/library classes (never reloaded)
 * - YummyClassLoader (child): loads user application classes (reloaded on changes)
 *
 * On file change:
 *   1. Create a new YummyClassLoader instance
 *   2. Old ClassLoader becomes unreachable and gets GC'd
 *   3. New loader picks up the updated .class files
 *
 * This approach is transparent to the user - no interfaces to implement.
 */
public class YummyClassLoader extends URLClassLoader {

    // Track which classes were loaded by this loader
    private final Set<String> loadedClasses = ConcurrentHashMap.newKeySet();

    // Packages that should always be loaded by the parent (not reloaded)
    private static final String[] PARENT_FIRST_PACKAGES = {
        "java.", "javax.", "sun.", "jdk.",
        "org.slf4j.", "org.apache.logging.",
        "dev.yummy.agent."
    };

    public YummyClassLoader(URL[] urls, ClassLoader parent) {
        super(urls, parent);
    }

    @Override
    protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
        // Parent-first for system/framework classes
        for (String prefix : PARENT_FIRST_PACKAGES) {
            if (name.startsWith(prefix)) {
                return super.loadClass(name, resolve);
            }
        }

        // Child-first for user classes: try to load from our URLs first
        synchronized (getClassLoadingLock(name)) {
            Class<?> loaded = findLoadedClass(name);
            if (loaded != null) {
                return loaded;
            }

            try {
                Class<?> c = findClass(name);
                loadedClasses.add(name);
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            } catch (ClassNotFoundException e) {
                // Fall back to parent
                return super.loadClass(name, resolve);
            }
        }
    }

    /**
     * Get the set of class names loaded by this loader.
     */
    public Set<String> getLoadedClassNames() {
        return Set.copyOf(loadedClasses);
    }

    @Override
    public void close() throws IOException {
        loadedClasses.clear();
        super.close();
    }
}
