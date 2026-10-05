def test_py_typed_marker_present() -> None:
    """PEP 561 marker must ship with the installed package."""
    import importlib.resources, tstrans
    pkg_files = importlib.resources.files(tstrans)
    assert (pkg_files / "py.typed").is_file(), (
        "py.typed marker missing — type checkers will treat tstrans as untyped"
    )
