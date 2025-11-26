#!/usr/bin/env python3
"""Check project license declarations, not complete distribution compliance.

Requires Python 3.11+. Uses Git selection or a verified frozen input manifest.
Historical review evidence and third-party sources are not relicensed.
"""
import argparse
import hashlib
import json
from pathlib import Path
import sys
import tomllib
import xml.etree.ElementTree as ET

import project_inputs


SPDX = 'AGPL-3.0-only'
LICENSE_SHA256 = '0d96a4ff68ad6d4b6f1f30f713b18d5184912ba8dd389f86aa7710db079abcb0'
LICENSE_COPIES = ('LICENSE', 'docs/LICENSE.md', 'interop/python/LICENSE',
                  'interop/typescript-lite/LICENSE')
THIRD_PARTY = {
    'vendor/tokio/LICENSE': '253cd04c6714889df2d32f3f64d669179a1c95c76ac43c40882c52eb06bc3552',
    'vendor/flume/LICENSE-MIT': '30fefc3a7d6a0041541858293bcbea2dde4caa4c0a5802f996a7f7e8c0085652',
    'vendor/flume/LICENSE-APACHE': 'd8621ec2eee5b9ca3c65f8492505f3590de8574fac0fdf5132a77232dba47abc',
    'vendor/rumqttc/LICENSE': 'cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30',
    'interop/cpp-lite/test_vendor/nlohmann/LICENSE.MIT':
        '46a65cffd1ea955132d95a8dd921640714a8d6b537d2e4e482d31145ae95b603',
}
THIRD_PARTY_PREFIXES = ('vendor/', 'interop/cpp-lite/test_vendor/')
REQUIRED_MANIFESTS = ('Cargo.toml', 'interop/python/pyproject.toml',
                      'interop/typescript-lite/package.json',
                      'interop/typescript-lite/package-lock.json',
                      'interop/jvm/pom.xml', 'apps/crawler-lab/jvm/pom.xml',
                      'apps/websearch/jvm/config-dependencies.pom.xml')
POM_NS = {'m': 'http://maven.apache.org/POM/4.0.0'}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def evaluate(root, selected):
    root = Path(root).resolve()
    selected = set(selected)
    checked = {}

    def file(name):
        require(name in selected, 'required license input not selected: ' + name)
        path = project_inputs.regular_file(root, name)
        checked[name] = project_inputs.identity(path)['sha256']
        return path

    def toml(name):
        return tomllib.loads(file(name).read_text(encoding='utf-8'))

    for name in REQUIRED_MANIFESTS:
        file(name)
    for name in LICENSE_COPIES:
        file(name)
        require(checked[name] == LICENSE_SHA256, 'nonstandard or stale license text: ' + name)
    for name, expected in THIRD_PARTY.items():
        file(name)
        require(checked[name] == expected, 'third-party license changed: ' + name)

    counts = {'rust_packages': 0, 'python_packages': 0, 'npm_packages': 0, 'maven_projects': 0}
    manifests = sorted(name for name in selected
                       if not name.startswith(THIRD_PARTY_PREFIXES)
                       and 'node_modules' not in Path(name).parts
                       and not project_inputs.excluded(name))
    for name in manifests:
        base = Path(name).name
        if base == 'Cargo.toml':
            data = toml(name)
            package = data.get('package')
            if package is None:
                continue
            require(package.get('license') == SPDX, 'incorrect Rust license: ' + name)
            license_file = package.get('license-file')
            require(isinstance(license_file, str) and bool(license_file),
                    'missing Rust license-file: ' + name)
            require((root / name).parent.joinpath(license_file).resolve() == root / 'LICENSE',
                    'Rust license-file must resolve to the project LICENSE: ' + name)
            counts['rust_packages'] += 1
        elif base == 'pyproject.toml':
            data = toml(name)
            if 'project' not in data:
                continue
            require(data['project'].get('license') == SPDX, 'incorrect Python license: ' + name)
            require(data['project'].get('license-files') == ['LICENSE'],
                    'missing Python license-files: ' + name)
            license_name = str(Path(name).parent / 'LICENSE')
            file(license_name)
            require(checked[license_name] == LICENSE_SHA256, 'incorrect Python LICENSE: ' + name)
            counts['python_packages'] += 1
        elif base == 'package.json':
            data = json.loads(file(name).read_text(encoding='utf-8'),
                              object_pairs_hook=project_inputs.no_duplicate_keys)
            require(data.get('license') == SPDX, 'incorrect npm license: ' + name)
            directory = Path(name).parent
            license_name = str(directory / 'LICENSE')
            file(license_name)
            require(checked[license_name] == LICENSE_SHA256, 'incorrect npm LICENSE: ' + name)
            lock_name = str(directory / 'package-lock.json')
            lock = json.loads(file(lock_name).read_text(encoding='utf-8'),
                              object_pairs_hook=project_inputs.no_duplicate_keys)
            require(lock.get('packages', {}).get('', {}).get('license') == SPDX,
                    'incorrect npm lockfile root license: ' + lock_name)
            counts['npm_packages'] += 1
        elif base.endswith('pom.xml'):
            pom = ET.parse(file(name)).getroot()
            licenses = pom.findall('m:licenses/m:license', POM_NS)
            require(len(licenses) == 1 and licenses[0].findtext('m:name', namespaces=POM_NS) == SPDX,
                    'incorrect Maven license: ' + name)
            require(licenses[0].findtext('m:url', namespaces=POM_NS)
                    == 'https://www.gnu.org/licenses/agpl-3.0.html', 'incorrect Maven license URL: ' + name)
            if pom.findtext('m:packaging', default='jar', namespaces=POM_NS) == 'jar':
                resources = pom.findall('m:build/m:resources/m:resource', POM_NS)
                found = False
                for resource in resources:
                    directory = resource.findtext('m:directory', default='', namespaces=POM_NS)
                    directory = directory.replace('${project.basedir}', str((root / name).parent))
                    if (resource.findtext('m:targetPath', namespaces=POM_NS) == 'META-INF'
                            and 'LICENSE' in [item.text for item in
                                             resource.findall('m:includes/m:include', POM_NS)]
                            and Path(directory).resolve() == root):
                        found = True
                # The websearch dependency-resolution POM does not produce a jar.
                require(found or name == 'apps/websearch/jvm/config-dependencies.pom.xml',
                        'missing Maven packaged license resource: ' + name)
            counts['maven_projects'] += 1
    require(all(counts.values()), 'missing language package coverage')
    return {'schema': 'parrot-license-consistency-v1', 'success': True,
            'license': SPDX, 'counts': counts, 'inputs': dict(sorted(checked.items())),
            'limitations': ['Not an SBOM, vulnerability scan, dependency compatibility audit, '
                            'published artifact or network-service compliance certification.']}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument('--manifest', type=Path)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    try:
        if args.manifest:
            manifest = project_inputs.load_manifest(args.manifest)
            project_inputs.verify(args.root, manifest, exact=True)
            selected = manifest['files']
        else:
            selected = project_inputs.discover(args.root)
        report = evaluate(args.root, selected)
        report['checker_sha256'] = project_inputs.identity(Path(__file__))['sha256']
        text = json.dumps(report, indent=2, ensure_ascii=False) + '\n'
        if args.output:
            with args.output.open('x', encoding='utf-8') as handle:
                handle.write(text)
        print(text, end='')
        return 0
    except (OSError, ValueError, ET.ParseError) as error:
        print('License consistency check failed: ' + str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
