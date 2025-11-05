# Test-only JSON dependency

nlohmann/json v3.12.0 single_include/nlohmann/json.hpp and LICENSE.MIT are
unmodified upstream files, fetched from the pinned Git tag. They are included
only by the native shared-vector tests, not parrot_lite.cpp or its public API.
The vendored header makes those tests compile without network access or a
machine-specific package installation.

Header SHA256: aaf127c04cb31c406e5b04a63f1ae89369fccde6d8fa7cdda1ed4f32dfc5de63
License SHA256: 46a65cffd1ea955132d95a8dd921640714a8d6b537d2e4e482d31145ae95b603

Source: https://github.com/nlohmann/json/tree/v3.12.0
