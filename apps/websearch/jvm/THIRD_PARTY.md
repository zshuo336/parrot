# Search configuration parser dependencies

The search component jar includes these pinned Maven Central artifacts. Their
SHA-256 digests are recorded in `config-dependencies.sha256` and checked before
compilation and packaging. `config-dependencies.pom.xml` records the dependency
closure; the build uses Maven Dependency Plugin 3.8.1.

- `org.tomlj:tomlj:1.1.1`: Apache License 2.0; https://github.com/tomlj/tomlj
- `org.antlr:antlr4-runtime:4.11.1`: BSD 3-Clause; https://github.com/antlr/antlr4/tree/4.11.1
- `org.checkerframework:checker-qual:3.21.2`: MIT License; https://checkerframework.org/

License texts are in `licenses/` and shipped in `META-INF/licenses/` of the
component jar. This supplements the existing bundled jieba-analysis 1.0.2
dependency; it does not require additional jars on deployed gateways.
