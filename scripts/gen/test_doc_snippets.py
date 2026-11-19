import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import doc_snippets as ds  # noqa: E402


def page(text):
    d = tempfile.mkdtemp()
    p = os.path.join(d, "demo.md")
    with open(p, "w") as fh:
        fh.write(text)
    return p, os.path.join(d, "out")


def read(path):
    with open(path) as fh:
        return fh.read()


class ExtractTests(unittest.TestCase):
    def test_java_statements_wrapped_and_numbered(self):
        p, out = page(
            "x\n\n<!-- compile: java-statements -->\n```java\nint a = 1;\n```\n\n"
            "<!-- compile: java-statements -->\n```java\nint b = 2;\n```\n"
        )
        w = ds.extract([p], out)
        self.assertEqual([os.path.basename(f) for f in w], ["demo-1.java", "demo-2.java"])
        t = read(w[0])
        self.assertIn("import org.tstrans.codec.*;", t)
        self.assertIn("static void run() throws Exception {", t)
        self.assertIn("int a = 1;", t)
        self.assertNotIn("int b", t)

    def test_unmarked_fence_ignored(self):
        p, out = page("```java\nint a;\n```\n```c\nint b;\n```\n")
        self.assertEqual(ds.extract([p], out), [])

    def test_marker_inside_a_fence_ignored(self):
        p, out = page("```md\n<!-- compile: c-file -->\n```\n")
        self.assertEqual(ds.extract([p], out), [])

    def test_marker_without_fence_is_error(self):
        p, out = page("<!-- compile: c-file -->\n\ntext\n")
        with self.assertRaises(ds.SnippetError):
            ds.extract([p], out)

    def test_marker_with_wrong_language_is_error(self):
        p, out = page("<!-- compile: c-file -->\n```java\nint a;\n```\n")
        with self.assertRaises(ds.SnippetError):
            ds.extract([p], out)

    def test_unknown_marker_is_error(self):
        p, out = page("<!-- compile: rust -->\n```rust\n```\n")
        with self.assertRaises(ds.SnippetError):
            ds.extract([p], out)

    def test_c_file_written_as_is(self):
        p, out = page("<!-- compile: c-file -->\n```c\nint main(void){return 0;}\n```\n")
        w = ds.extract([p], out)
        self.assertEqual(read(w[0]), "int main(void){return 0;}\n")

    def test_c_statements_wrapped(self):
        p, out = page("<!-- compile: c-statements -->\n```c\nint a = 1;\n```\n")
        t = read(ds.extract([p], out)[0])
        self.assertTrue(t.startswith('#include "tstrans.h"'))
        self.assertIn("int snippet_demo_1(void) {\nint a = 1;\n    return 0;\n}", t)

    def test_continues_joins_previous_statements(self):
        p, out = page(
            "<!-- compile: java-statements -->\n```java\nint a = 1;\n```\n"
            "<!-- compile: java-statements continues -->\n```java\nint b = a;\n```\n"
        )
        w = ds.extract([p], out)
        self.assertEqual(len(w), 1)
        self.assertIn("int b = a;", read(w[0]))

    def test_continues_without_parent_is_error(self):
        p, out = page("<!-- compile: java-statements continues -->\n```java\nint b;\n```\n")
        with self.assertRaises(ds.SnippetError):
            ds.extract([p], out)

    def test_java_imports_hoisted(self):
        p, out = page("<!-- compile: java-statements -->\n```java\nimport java.util.Map;\nint a;\n```\n")
        t = read(ds.extract([p], out)[0])
        self.assertLess(t.index("import java.util.Map;"), t.index("class "))

    def test_prelude_goes_into_wrapper_and_hoists_imports(self):
        p, out = page(
            "<!-- compile-prelude: java\nimport java.util.Map;\nstatic byte[] klvBytes;\n-->\n"
            "<!-- compile-prelude: c\nconst char *g;\n-->\n"
            "<!-- compile: java-statements -->\n```java\nint a = klvBytes.length;\n```\n"
            "<!-- compile: c-statements -->\n```c\nint a = 1;\n```\n"
        )
        j, c = [read(f) for f in ds.extract([p], out)]
        self.assertLess(j.index("import java.util.Map;"), j.index("class "))
        self.assertLess(j.index("class "), j.index("static byte[] klvBytes"))
        self.assertLess(j.index("static byte[] klvBytes"), j.index("static void run"))
        self.assertLess(c.index("const char *g;"), c.index("int snippet_"))

    def test_unclosed_prelude_is_error(self):
        p, out = page("<!-- compile-prelude: java\nint a;\n")
        with self.assertRaises(ds.SnippetError):
            ds.extract([p], out)

    def test_java_class_named_after_public_type(self):
        p, out = page("<!-- compile: java-class -->\n```java\npublic final class Foo {}\n```\n")
        w = ds.extract([p], out)
        self.assertTrue(w[0].endswith(os.path.join("demo-1", "Foo.java")))


if __name__ == "__main__":
    unittest.main()
