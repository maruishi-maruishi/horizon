"""Prepare what a Horizon server needs to let vanilla clients of older releases join
(docs/older-clients.md): for each release, the data-driven registry entries its own client
jar has, and one entry's data per registry as the stand-in for entries that release lacks.

usage: python -I tools/prepare_older_clients.py <resources-dir> <client.jar> [<client.jar> ...]

Writes <resources-dir>/older/<protocol>.json. The output is Mojang's data: keep it in the
local resources folder, never in the repository.
"""
import json
import os
import re
import sys
import zipfile

# The release each client jar's protocol maps to and the core pack versions clients of that
# protocol report (patch releases share a protocol but not a core pack version).
PROTOCOLS = {
    '1.21': 767, '1.21.1': 767, '1.21.2': 768, '1.21.3': 768, '1.21.4': 769, '1.21.5': 770,
    '1.21.6': 771, '1.21.7': 772, '1.21.8': 772, '1.21.9': 773, '1.21.10': 773, '1.21.11': 774,
    '26.1': 775, '26.1.1': 775, '26.1.2': 775, '26.2': 776,
}
SKIP = re.compile(r'^(tags|recipes?|advancements?|loot_tables?|structures?|functions?|datapacks)(/|$)')

resources, jars = sys.argv[1], sys.argv[2:]
out_dir = os.path.join(resources, 'older')
os.makedirs(out_dir, exist_ok=True)

for jar in jars:
    with zipfile.ZipFile(jar) as z:
        version = json.loads(z.read('version.json'))
        release = version.get('id') or version['name']
        protocol = version.get('protocol_version') or PROTOCOLS[release]
        registries = {}
        # Tags by registry (`tags/item/enchantable/sword.json` is item tag `enchantable/sword`): the
        # release's own data (enchantments...) names tags the pinned release may have renamed.
        tags = {}
        for name in sorted(z.namelist()):
            m = re.match(r'^data/minecraft/tags/(.+?)/(.+)\.json$', name)
            if not m:
                continue
            reg, tag = m.group(1), m.group(2)
            # `worldgen/biome` and other two-level registries keep their folder.
            if reg == 'worldgen':
                first, _, rest = tag.partition('/')
                reg, tag = f'worldgen/{first}', rest
            values = json.loads(z.read(name)).get('values', [])
            tags.setdefault('minecraft:' + reg, {})['minecraft:' + tag] = values
        for name in sorted(z.namelist()):
            m = re.match(r'^data/minecraft/(.+)/([^/]+)\.json$', name)
            if not m or SKIP.match(m.group(1)):
                continue
            # A registry's files may be nested (`worldgen/biome/...` stays one level here).
            key, entry = m.group(1), m.group(2)
            reg = registries.setdefault('minecraft:' + key, {'entries': [], 'standin': None, 'standin_name': None})
            reg['entries'].append('minecraft:' + entry)
            if reg['standin'] is None:
                reg['standin'] = json.loads(z.read(name))
                reg['standin_name'] = 'minecraft:' + entry
    versions = sorted(r for r, p in PROTOCOLS.items() if p == protocol)
    path = os.path.join(out_dir, f'{protocol}.json')
    with open(path, 'w', encoding='utf-8') as f:
        json.dump({'release': release, 'protocol': protocol, 'core_versions': versions,
                   'registries': registries, 'tags': tags}, f, separators=(',', ':'))
    print(f'{release} (protocol {protocol}): {len(registries)} registries -> {path}')
