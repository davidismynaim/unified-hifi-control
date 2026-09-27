/**
 * ServerBrokerID derivation, confirmed against a live Core and cross-checked
 * against its own ConnectResponse::BrokerId field.
 *
 * Roon Core is a .NET app internally: it serializes its own Guid using
 * .NET's native byte layout (Guid.ToByteArray()), which byte-swaps the
 * first three fields (4+2+2 bytes) to little-endian and leaves the last 8
 * bytes as-is. The SOOD discovery `unique_id` is the SAME guid printed in
 * the conventional big-endian dashed form - naively stripping the dashes
 * and using those bytes gets a `ROON 01 81` rejection from the handshake.
 */
export function serverBrokerIdFromUniqueId(uniqueId: string): Buffer {
  const hex = uniqueId.replace(/-/g, '');
  if (hex.length !== 32) throw new Error(`not a GUID: "${uniqueId}"`);
  const bytes = Buffer.from(hex, 'hex');
  // RFC4122 field order: time_low(4) time_mid(2) time_hi_and_version(2) rest(8)
  const timeLow = bytes.subarray(0, 4);
  const timeMid = bytes.subarray(4, 6);
  const timeHi = bytes.subarray(6, 8);
  const rest = bytes.subarray(8, 16);
  return Buffer.concat([
    Buffer.from(timeLow).reverse(),
    Buffer.from(timeMid).reverse(),
    Buffer.from(timeHi).reverse(),
    rest,
  ]);
}

/** .NET `DateTime.ToBinary()` int64: low 62 bits are ticks (100ns since 0001-01-01). */
const DOTNET_EPOCH_TO_UNIX_EPOCH_TICKS = 621355968000000000n;
const TICKS_PER_MS = 10000n;

export function decodeDotNetDateTimeBinary(binary: bigint): Date | undefined {
  const ticks = binary & 0x3fffffffffffffffn;
  if (ticks === 0n) return undefined;
  const unixMs = (ticks - DOTNET_EPOCH_TO_UNIX_EPOCH_TICKS) / TICKS_PER_MS;
  return new Date(Number(unixMs));
}

/**
 * `Sooloos.NullDate` (RoonBase.dll, decompiled with ilspycmd from the
 * official Windows client's Roon.Broker.Api.dll/RoonBase.dll):
 *
 *   public int ToBinary() => (_year << 16) | (_month << 8) | _day;
 *
 * The wire does NOT send that as a raw 4-byte int (RemotingUtils.WriteInteger) -
 * it flex-encodes it, same as every other integer in this protocol (7 bits/byte,
 * continuation bit on all but the last byte). Confirmed byte-exact against two
 * independently known release dates (King Crimson "Red" = 1974-10-06, Taylor
 * Swift "Red (Taylor's Version)" = 2021-11-12) captured live from this Core.
 */
export interface NullDate {
  year: number;
  month: number; // 0 = unknown
  day: number; // 0 = unknown
}

export function decodeNullDate(buf: Buffer | undefined): NullDate | undefined {
  if (!buf || buf.length === 0) return undefined;
  let raw = 0;
  for (const b of buf) {
    raw = (raw << 7) | (b & 0x7f);
    if ((b & 0x80) === 0) break;
  }
  const year = (raw >>> 16) & 0xffff;
  if (year === 0) return undefined;
  return { year, month: (raw >>> 8) & 0xff, day: raw & 0xff };
}
