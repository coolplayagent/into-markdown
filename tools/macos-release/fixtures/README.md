# Office 97–2003 corpus

These DOC, PPT, and XLS files are repository-authored acceptance inputs. They are licensed under
Apache-2.0 with the project and are not copied from anydoc or another parser project.

`manifest.json` fixes the byte size, SHA-256, license, and intended coverage. Rebuild or verify the
checked-in bytes without LibreOffice, network access, or another Office implementation:

```sh
python3 tools/macos-release/fixtures/generate.py --output /tmp/legacy-office-corpus
python3 tools/macos-release/fixtures/generate.py --check
```
