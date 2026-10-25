#!/usr/bin/env python3
"""Decode this HCMD escaped-wide-header capture, without transmitting anything.

Python 3.9+; no dependencies. Input is a text file with HCMD lines.
The format has no extra length prefix: <BBHHBB>, followed by payload.
Bytes after the advertised payload are capture padding, not commands.
"""
import argparse
import json
import re
import struct
from pathlib import Path

NAMES = {
    (1, 8): 'PHY_CONTEXT_CMD',
    (1, 0x0c): 'SCAN_CFG_CMD',
    (1, 0x0d): 'SCAN_REQ_UMAC',
    (1, 0x1e): 'TXPATH_FLUSH',
    (1, 0x91): 'BEACON_TEMPLATE_CMD',
    (1, 0xa9): 'MAC_PM_POWER_TABLE',
    (1, 0xd2): 'BEACON_FILTERING_CMD',
    (2, 0): 'SHARED_MEM_CFG_CMD',
    (2, 1): 'SOC_CONFIGURATION_CMD',
    (2, 3): 'INIT_EXTENDED_CFG_CMD',
    (2, 15): 'SYSTEM_STATISTICS_CMD',
    (3, 8): 'MAC_CONFIG_CMD',
    (3, 9): 'LINK_CONFIG_CMD',
    (3, 10): 'STA_CONFIG_CMD',
    (3, 12): 'STA_REMOVE_CMD',
    (5, 2): 'TRIGGER_RX_QUEUES_NOTIF_CMD',
    (5, 8): 'RLC_CONFIG_CMD',
    (5, 13): 'RFH_QUEUE_CONFIG_CMD',
    (5, 15): 'TLC_MNG_CONFIG_CMD',
    (5, 22): 'RX_BAID_ALLOCATION_CONFIG_CMD',
    (5, 23): 'SCD_QUEUE_CONFIG_CMD',
    (5, 24): 'SEC_KEY_CMD',
}


def unescape(text):
    """Decode only \\xHH. Preserve all other literal printable characters."""
    result = bytearray()
    i = 0
    while i < len(text):
        if text[i:i+2] == '\\x':
            digits = text[i+2:i+4]
            if len(digits) != 2 or not re.fullmatch('[0-9a-fA-F]{2}', digits):
                raise ValueError('malformed hexadecimal escape at character %d' % i)
            result.append(int(digits, 16))
            i += 4
        else:
            if ord(text[i]) > 255:
                raise ValueError('non-byte character at character %d' % i)
            result.append(ord(text[i]))
            i += 1
    return bytes(result)


def decode(text):
    commands = []
    for line_number, line in enumerate(text.splitlines(), 1):
        if not line.startswith('HCMD '):
            continue
        raw = unescape(line[5:])
        if len(raw) < 8:
            raise ValueError('line %d: incomplete command header' % line_number)
        opcode, group, sequence, length, reserved, version = struct.unpack_from('<BBHHBB', raw)
        available = len(raw) - 8
        payload = raw[8:8+length]
        entry = dict(index=len(commands), line=line_number,
                     command='%02x.%02x' % (group, opcode),
                     name=NAMES.get((group, opcode), 'UNRESOLVED'),
                     group=group, opcode=opcode, sequence=sequence,
                     header_version=version, reserved=reserved,
                     advertised_length=length, captured_length=len(payload),
                     capture_record_length=len(raw), truncated=length > available,
                     missing_bytes=max(0, length-available),
                     trailing_capture_bytes=max(0, available-length),
                     payload_hex=payload.hex())
        def u32(offset):
            return struct.unpack_from('<I', payload, offset)[0]
        if (group, opcode) == (3, 8) and len(payload) >= 12:
            entry['fields'] = dict(id_and_color=u32(0), action=u32(4), mac_type=u32(8))
        elif (group, opcode) == (3, 9) and len(payload) >= 32:
            entry['fields'] = dict(action=u32(0), link_id=u32(4), mac_id=u32(8),
                                   phy_id=u32(12), modify_mask=u32(24), active=u32(28))
        elif (group, opcode) == (3, 10) and len(payload) >= 28:
            entry['fields'] = dict(sta_id=u32(0), link_id=u32(4), station_type=u32(24))
        elif (group, opcode) == (5, 23) and len(payload) >= 4:
            fields = dict(operation=u32(0))
            if u32(0) == 0 and len(payload) >= 36:
                fields.update(sta_mask=u32(4), tid=payload[8], flags=u32(12), cb_size=u32(16))
            elif u32(0) == 1 and len(payload) >= 12:
                fields.update(sta_mask=u32(4), tid=u32(8))
            entry['fields'] = fields
        commands.append(entry)
    if not commands:
        raise ValueError('no HCMD records found')
    return commands


def compare_templates(commands, source):
    mapping = dict(mac_add=82, link_add=83, rlc1=86, link_mod1=87,
                   link_mod2=88, cmd_9f=89, rlc2=91, beacon=92,
                   link_mod3=93, sta_mcast=94, sta_bcast=96, link_mod4=98)
    comparisons = []
    for name, index in mapping.items():
        match = re.search(r'static const uint8_t mld_' + name +
                          r'\[(\d+)\]\s*=\s*\{(.*?)\};', source, re.S)
        if not match or index >= len(commands):
            raise ValueError('missing template or capture index: ' + name)
        values = bytes(int(s, 16) for s in re.findall(r'0x([0-9a-fA-F]{2})', match[2]))
        declared = int(match[1])
        if len(values) > declared:
            raise ValueError('template initializer too long: ' + name)
        values += b'\0' * (declared - len(values))
        command = commands[index]
        comparisons.append(dict(template='mld_'+name, index=index,
                                payload_equal=not command['truncated'] and
                                values.hex() == command['payload_hex'],
                                captured_header_version=command['header_version']))
    return comparisons


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('capture', type=Path)
    parser.add_argument('--source', type=Path, help='optional awdl-code-excerpt.cpp')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    commands = decode(args.capture.read_text(encoding='utf-8'))
    result = dict(format='iwlwifi-wide-header', commands=commands,
                  summary=dict(count=len(commands),
                               truncated_indices=[c['index'] for c in commands if c['truncated']],
                               versioned_indices=[c['index'] for c in commands if c['header_version']]))
    if args.source:
        result['template_comparison'] = compare_templates(commands, args.source.read_text(encoding='utf-8'))
    args.output.write_text(json.dumps(result, indent=2, ensure_ascii=False)+'\n', encoding='utf-8')
    print(json.dumps(result['summary'], ensure_ascii=False))
    # Exit 2 signals incomplete payloads; the diagnostic JSON is still valid.
    return 2 if result['summary']['truncated_indices'] else 0


if __name__ == '__main__':
    raise SystemExit(main())
