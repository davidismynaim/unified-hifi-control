/**
 * Mirrors src/mqtt/topics.rs and src/mqtt/discovery.rs in the main UHC
 * binary, so entities published here attach to the *same* HA device UHC
 * already created for a zone, without touching any topic UHC itself owns.
 */

export function zoneSlug(zoneId: string): string {
  return zoneId.replace(/[^a-zA-Z0-9]/g, '_');
}

/** Our own state topic - distinct from UHC's `media_player/<slug>/state`. */
export function ourStateTopic(baseTopic: string, zoneId: string): string {
  return `${baseTopic}/roon_swim/${zoneSlug(zoneId)}/state`;
}

/** Our own bridge availability topic - distinct from UHC's `bridge/status`. */
export function ourAvailabilityTopic(baseTopic: string): string {
  return `${baseTopic}/roon_swim_bridge/status`;
}

/** Same discovery topic shape UHC uses, so entities list together in HA's MQTT integration. */
export function discoveryTopic(
  discoveryPrefix: string,
  component: string,
  zoneId: string,
  entitySuffix: string
): string {
  return `${discoveryPrefix}/${component}/unified_hifi_control/${zoneSlug(zoneId)}_${entitySuffix}/config`;
}

/** Same device grouping key UHC's discovery.rs uses (`uhc_<zone_slug>`), so
 * these show up as more entities on the device UHC already created. */
export function deviceFor(zoneId: string, zoneName: string, source: string) {
  return {
    identifiers: [`uhc_${zoneSlug(zoneId)}`],
    name: zoneName,
    manufacturer: 'Unified Hi-Fi Control',
    model: source,
    via_device: 'unified_hifi_control',
  };
}
