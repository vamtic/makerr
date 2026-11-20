//! Template JSON files: the same schema the V1 app's "Export Template"
//! writes and `--headless` consumes, plus import/export helpers.

use std::path::Path;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::coord::Region;

/// A render job template.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub canvas_id: String,
    pub first_cord: String,
    pub second_cord: String,
    /// `yyyy-MM-dd`
    pub start_date: String,
    /// `yyyy-MM-dd`
    pub end_date: String,
    pub output_folder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_domain: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("cannot open template: {0}")]
    Io(#[from] std::io::Error),
    #[error("template is missing 'canvas_id'")]
    MissingCanvasId,
    #[error("invalid coordinate pair: {0}")]
    BadCoords(String),
    #[error("invalid date '{0}' (expected yyyy-MM-dd)")]
    BadDate(String),
    #[error("start date is after end date")]
    DateOrder,
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl Template {
    pub fn load(path: impl AsRef<Path>) -> Result<Template, TemplateError> {
        let data = std::fs::read(path)?;
        let template: Template = serde_json::from_slice(&data)?;
        // A reversed range would silently produce an empty work list; reject it
        // at the boundary instead of reporting "0 frames" after a download.
        if let (Ok(start), Ok(end)) = (template.start(), template.end()) {
            if start > end {
                return Err(TemplateError::DateOrder);
            }
        }
        Ok(template)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), TemplateError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Normalized region from the two corner strings (order-independent).
    pub fn region(&self) -> Result<Region, TemplateError> {
        crate::coord::region_from_corners(&self.first_cord, &self.second_cord).ok_or_else(|| {
            TemplateError::BadCoords(format!("{}, {}", self.first_cord, self.second_cord))
        })
    }

    pub fn start(&self) -> Result<NaiveDate, TemplateError> {
        NaiveDate::parse_from_str(&self.start_date, "%Y-%m-%d")
            .map_err(|_| TemplateError::BadDate(self.start_date.clone()))
    }

    pub fn end(&self) -> Result<NaiveDate, TemplateError> {
        NaiveDate::parse_from_str(&self.end_date, "%Y-%m-%d")
            .map_err(|_| TemplateError::BadDate(self.end_date.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_v1_style_template() {
        let json = r#"{
            "canvas_id": "b740af83",
            "first_cord": "-128_-128",
            "second_cord": "128_128",
            "start_date": "2000-01-01",
            "end_date": "2000-01-07",
            "output_folder": "/tmp/out/"
        }"#;
        let t: Template = serde_json::from_str(json).unwrap();
        assert_eq!(t.canvas_id, "b740af83");
        assert!(t.domain.is_none());
        let r = t.region().unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (-128, -128, 257, 257));
        assert_eq!(
            t.start().unwrap(),
            NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()
        );
        assert_eq!(
            t.end().unwrap(),
            NaiveDate::from_ymd_opt(2000, 1, 7).unwrap()
        );
    }

    #[test]
    fn round_trip_preserves_site_fields() {
        let t = Template {
            canvas_id: "0".into(),
            first_cord: "0_0".into(),
            second_cord: "10_10".into(),
            start_date: "2000-01-01".into(),
            end_date: "2000-01-02".into(),
            output_folder: "out".into(),
            domain: Some("pixmap.fun".into()),
            storage_domain: Some("pixmap.fun/history/chunk".into()),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.json");
        t.save(&path).unwrap();
        let loaded = Template::load(&path).unwrap();
        assert_eq!(loaded, t);
    }

    #[test]
    fn load_rejects_a_reversed_date_range() {
        let t = Template {
            canvas_id: "0".into(),
            first_cord: "0_0".into(),
            second_cord: "1_1".into(),
            start_date: "2000-02-01".into(),
            end_date: "2000-01-01".into(),
            output_folder: "out".into(),
            domain: None,
            storage_domain: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.json");
        t.save(&path).unwrap();
        assert!(matches!(
            Template::load(&path),
            Err(TemplateError::DateOrder)
        ));
    }

    #[test]
    fn rejects_bad_input() {
        let mk = |first: &str| {
            serde_json::from_str::<Template>(&format!(
            r#"{{"canvas_id":"c","first_cord":"{first}","second_cord":"1_1","start_date":"2000-01-01","end_date":"2000-01-02","output_folder":"o"}}"#
        ))
        .unwrap()
        };
        assert!(matches!(mk("0").region(), Err(TemplateError::BadCoords(_))));
        assert!(matches!(
            mk("a_b").region(),
            Err(TemplateError::BadCoords(_))
        ));

        let mut bad = mk("0_0");
        bad.start_date = "01/02/2000".into();
        assert!(matches!(bad.start(), Err(TemplateError::BadDate(_))));
    }
}
