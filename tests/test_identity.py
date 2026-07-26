from pathlib import Path

from runlab.identity import digest_directory


def test_directory_digest_is_stable_and_content_sensitive(tmp_path: Path) -> None:
    (tmp_path / "a.txt").write_text("alpha")
    nested = tmp_path / "nested"
    nested.mkdir()
    (nested / "b.txt").write_text("beta")

    first = digest_directory(tmp_path)
    assert digest_directory(tmp_path) == first

    (nested / "b.txt").write_text("changed")
    assert digest_directory(tmp_path) != first


def test_directory_digest_includes_hidden_files(tmp_path: Path) -> None:
    (tmp_path / "Dockerfile").write_text("FROM scratch")
    before = digest_directory(tmp_path)

    (tmp_path / ".hidden").write_text("included")
    assert digest_directory(tmp_path) != before
