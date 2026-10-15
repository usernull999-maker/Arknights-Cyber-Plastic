//! Bundled stage data and grid projection. Source commits and hashes live with
//! each map in resources/maps; selecting a stage never requires network access.

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub enum StageId {
    #[default]
    #[serde(rename = "level_crisis_v2_05-01")]
    MoranShuge,
    #[serde(rename = "main_08-17")]
    Jt8_3,
}

impl StageId {
    pub const ALL: [Self; 2] = [Self::MoranShuge, Self::Jt8_3];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MoranShuge => "level_crisis_v2_05-01",
            Self::Jt8_3 => "main_08-17",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "level_crisis_v2_05-01" | "墨染书阁" => Ok(Self::MoranShuge),
            "main_08-17" | "jt8-3" => Ok(Self::Jt8_3),
            _ => Err(format!("不支持的关卡：{value}")),
        }
    }

    pub fn map(self) -> &'static StageMap {
        static MORAN: OnceLock<StageMap> = OnceLock::new();
        static JT8_3: OnceLock<StageMap> = OnceLock::new();
        let (slot, json) = match self {
            Self::MoranShuge => (
                &MORAN,
                include_str!("../../../resources/maps/moran-shuge.json"),
            ),
            Self::Jt8_3 => (&JT8_3, include_str!("../../../resources/maps/jt8-3.json")),
        };
        slot.get_or_init(|| StageMap::from_json(json, self).expect("invalid bundled stage data"))
    }

    pub fn display_name(self) -> String {
        let map = self.map();
        if map.code.is_empty() {
            map.name.clone()
        } else {
            format!("{} {}", map.code, map.name)
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StageInfo {
    pub stage_id: StageId,
    pub name: String,
    pub width: i32,
    pub height: i32,
}

pub fn available_stages() -> Vec<StageInfo> {
    StageId::ALL
        .into_iter()
        .map(|stage_id| StageInfo {
            stage_id,
            name: stage_id.display_name(),
            width: stage_id.map().width,
            height: stage_id.map().height,
        })
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct MapSource {
    pub repository: String,
    pub commit: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
pub struct StageMap {
    pub schema_version: u32,
    pub stage_id: StageId,
    pub code: String,
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub normal_view: [f64; 3],
    pub side_view: [f64; 3],
    pub tiles: Vec<StageTile>,
    pub source: MapSource,
}

#[derive(Debug, Deserialize)]
pub struct StageTile {
    pub x: i32,
    pub y: i32,
    pub buildable_type: i32,
    pub height_type: i32,
    pub tile_key: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct PointI32 {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TileMatch {
    pub x: i32,
    pub y: i32,
    pub buildable_type: i32,
    pub tile_key: &'static str,
    pub screen: PointI32,
    pub distance_px: f64,
}

impl StageMap {
    fn from_json(json: &str, expected: StageId) -> Result<Self, String> {
        let map: Self = serde_json::from_str(json).map_err(|error| error.to_string())?;
        if map.schema_version != 1 || map.stage_id != expected {
            return Err("unsupported map schema or mismatched stage ID".into());
        }
        if map.width <= 0
            || map.height <= 0
            || map.width > 128
            || map.height > 128
            || map.tiles.len() != (map.width * map.height) as usize
        {
            return Err("invalid map dimensions".into());
        }
        for (index, tile) in map.tiles.iter().enumerate() {
            if tile.x != index as i32 % map.width
                || tile.y != index as i32 / map.width
                || tile.tile_key.is_empty()
            {
                return Err("map tiles must cover the grid in row-major order".into());
            }
        }
        if !map
            .normal_view
            .iter()
            .chain(map.side_view.iter())
            .all(|value| value.is_finite())
        {
            return Err("invalid camera parameters".into());
        }
        Ok(map)
    }

    pub(crate) fn snap(
        &'static self,
        client: PointI32,
        width: u32,
        height: u32,
        side: bool,
    ) -> Option<TileMatch> {
        self.tiles
            .iter()
            .filter(|tile| tile.tile_key != "tile_forbidden")
            .filter_map(|tile| {
                let screen = self.project(tile, width as f64, height as f64, side)?;
                let dx = f64::from(screen.x) - f64::from(client.x);
                let dy = f64::from(screen.y) - f64::from(client.y);
                Some(TileMatch {
                    x: tile.x,
                    y: tile.y,
                    buildable_type: tile.buildable_type,
                    tile_key: &tile.tile_key,
                    screen,
                    distance_px: dx.hypot(dy),
                })
            })
            .min_by(|lhs, rhs| lhs.distance_px.total_cmp(&rhs.distance_px))
    }

    pub fn project(
        &self,
        tile: &StageTile,
        width: f64,
        height: f64,
        side: bool,
    ) -> Option<PointI32> {
        if width <= 0.0 || height <= 0.0 || !width.is_finite() || !height.is_finite() {
            return None;
        }
        let ratio = height / width;
        let t = (9.0 / 16.0 - ratio) / (9.0 / 16.0 - 3.0 / 4.0);
        let view = if side {
            self.side_view
        } else {
            self.normal_view
        };
        let mut x = tile.x as f64 - (self.width as f64 - 1.0) / 2.0 - (view[0] - 1.4 * t);
        let mut y = (self.height as f64 - 1.0) / 2.0 - tile.y as f64 - (view[1] - 2.8 * t);
        let mut z = tile.height_type as f64 * -0.4 - view[2];
        if side {
            let yaw = 10.0_f64.to_radians();
            let next_x = yaw.cos() * x + yaw.sin() * z;
            z = -yaw.sin() * x + yaw.cos() * z;
            x = next_x;
        }
        let pitch = 30.0_f64.to_radians();
        let next_y = pitch.cos() * y - pitch.sin() * z;
        z = -pitch.sin() * y - pitch.cos() * z;
        y = next_y;
        if z == 0.0 {
            return None;
        }
        let tan_fov = 20.0_f64.to_radians().tan();
        let clip_x = (ratio / tan_fov) * x / -z;
        let clip_y = (1.0 / tan_fov) * y / -z;
        Some(PointI32 {
            x: ((clip_x + 1.0) * 0.5 * width).round() as i32,
            y: ((1.0 - ((clip_y + 1.0) * 0.5)) * height).round() as i32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jt8_3_has_its_own_complete_map_and_camera() {
        let stage = StageId::parse("JT8-3").unwrap();
        let map = stage.map();
        assert_eq!((map.width, map.height, map.tiles.len()), (15, 9, 135));
        assert_eq!(map.name, "昂首，足践烈焰");
        assert_eq!(map.normal_view, [0.0, -6.1, -9.7]);
        assert_ne!(map.side_view, StageId::MoranShuge.map().side_view);
        assert_eq!(map.tiles[3].tile_key, "tile_telin");
        assert_eq!(map.tiles[1].tile_key, "tile_forbidden");
    }

    #[test]
    fn stage_selection_is_explicit_and_rejects_unknown_maps() {
        assert_eq!(StageId::default(), StageId::MoranShuge);
        assert!(StageId::parse("JT8-2").is_err());
        assert!(serde_json::from_str::<StageId>("\"unknown\"").is_err());
        assert_eq!(
            serde_json::to_string(&StageId::Jt8_3).unwrap(),
            "\"main_08-17\""
        );
    }

    #[test]
    fn grid_centers_snap_back_in_both_views_and_aspect_ratios() {
        for stage in StageId::ALL {
            let map = stage.map();
            for (width, height) in [(1920, 1080), (1280, 720), (1440, 1080)] {
                for side in [false, true] {
                    for tile in map.tiles.iter().filter(|tile| tile.buildable_type != 0) {
                        let point = map
                            .project(tile, width as f64, height as f64, side)
                            .unwrap();
                        let matched = map.snap(point, width, height, side).unwrap();
                        assert_eq!(
                            (matched.x, matched.y),
                            (tile.x, tile.y),
                            "{stage:?}, {width}x{height}, side={side}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn malformed_map_data_is_rejected_instead_of_falling_back() {
        let json = include_str!("../../../resources/maps/jt8-3.json");
        assert!(StageMap::from_json(json, StageId::MoranShuge).is_err());
        let mut value: serde_json::Value = serde_json::from_str(json).unwrap();
        value["tiles"].as_array_mut().unwrap().pop();
        assert!(StageMap::from_json(&value.to_string(), StageId::Jt8_3).is_err());
        assert!(StageId::Jt8_3
            .map()
            .snap(PointI32 { x: 0, y: 0 }, 0, 1080, false)
            .is_none());
    }
}
