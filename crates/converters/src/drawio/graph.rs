use super::{budget::limit, model::Cell, output::Output};
use into_markdown_core::{ConversionError, ResourceReservation, SourceLocator};
use std::collections::BTreeMap;

pub(super) struct Graph<'a> {
    pub ids: BTreeMap<&'a str, Vec<usize>>,
    pub parents: Vec<Option<usize>>,
    pub children: Vec<Vec<usize>>,
    pub depths: Vec<usize>,
    pub labels: Vec<Vec<usize>>,
    pub _memory: ResourceReservation,
}

impl<'a> Graph<'a> {
    pub fn new(
        cells: &'a [Cell],
        locations: &[SourceLocator],
        out: &mut Output<'_>,
    ) -> Result<Self, ConversionError> {
        let memory = out.context.reserve_memory((cells.len() as u64).saturating_mul(1024))?;
        let mut ids: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (i, cell) in cells.iter().enumerate() {
            out.context.checkpoint()?;
            if cell.id().is_empty() {
                out.defect(
                    "drawio.missingId",
                    format!("Cell {} has no ID; assigned stable source ordinal", i + 1),
                    &locations[i],
                )?;
            } else {
                ids.entry(cell.id()).or_default().push(i);
            }
            if cell.edge() && cell.attr("vertex") == "1" {
                out.defect(
                    "drawio.conflictingCellKind",
                    "Cell is both edge and vertex; retained as edge".into(),
                    &locations[i],
                )?;
            }
        }
        for matches in ids.values().filter(|v| v.len() > 1) {
            for &i in matches {
                out.defect(
                    "drawio.duplicateId",
                    format!(
                        "Cell {} shares an ID with {} cells; ordinal identities retained",
                        i + 1,
                        matches.len()
                    ),
                    &locations[i],
                )?;
            }
        }
        let mut graph = Self {
            ids,
            parents: vec![None; cells.len()],
            children: vec![Vec::new(); cells.len()],
            depths: vec![0; cells.len()],
            labels: vec![Vec::new(); cells.len()],
            _memory: memory,
        };
        graph.resolve(cells, locations, out)?;
        graph.break_cycles(locations, out)?;
        for (i, cell) in cells.iter().enumerate() {
            out.context.checkpoint()?;
            let mut at = graph.parents[i];
            let mut owner = None;
            while let Some(parent) = at {
                out.context.checkpoint()?;
                if cells[parent].edge() {
                    owner = Some(parent);
                    break;
                }
                at = graph.parents[parent];
            }
            if let Some(edge) = owner.filter(|_| !cell.edge()) {
                graph.labels[edge].push(i);
            } else if let Some(parent) = graph.parents[i] {
                graph.children[parent].push(i);
            }
        }
        Ok(graph)
    }

    fn resolve(
        &mut self,
        cells: &[Cell],
        loc: &[SourceLocator],
        out: &mut Output<'_>,
    ) -> Result<(), ConversionError> {
        for (i, cell) in cells.iter().enumerate() {
            out.context.checkpoint()?;
            let parent = cell.attr("parent");
            if !parent.is_empty() {
                match self.ids.get(parent).map(Vec::as_slice) {
                    Some([p]) => self.parents[i] = Some(*p),
                    Some(_) => out.defect("drawio.ambiguousParent", "Parent ID is ambiguous; cell retained at page root with original reference".into(), &loc[i])?,
                    None => out.defect("drawio.missingParent", "Parent ID is missing; cell retained at page root with original reference".into(), &loc[i])?,
                }
            }
            if cell.edge() {
                for key in ["source", "target"] {
                    let value = cell.attr(key);
                    if value.is_empty() {
                        continue;
                    }
                    match self.ids.get(value).map(Vec::len) {
                        Some(1) => (),
                        Some(_) => out.defect(
                            "drawio.ambiguousEndpoint",
                            format!("Edge {key} ID is ambiguous; all candidates retained"),
                            &loc[i],
                        )?,
                        None => out.defect(
                            "drawio.danglingEndpoint",
                            format!("Edge {key} ID is missing; original reference retained"),
                            &loc[i],
                        )?,
                    }
                }
            }
        }
        Ok(())
    }

    fn break_cycles(
        &mut self,
        loc: &[SourceLocator],
        out: &mut Output<'_>,
    ) -> Result<(), ConversionError> {
        let mut colors = vec![0u8; self.parents.len()];
        let mut path = Vec::new();
        for i in 0..self.parents.len() {
            out.context.checkpoint()?;
            if colors[i] == 2 {
                continue;
            }
            path.clear();
            let mut at = Some(i);
            while let Some(n) = at {
                out.context.checkpoint()?;
                if colors[n] == 2 {
                    break;
                }
                if colors[n] == 1 {
                    let last = *path
                        .last()
                        .ok_or_else(|| limit("drawio_graph", "invalid parent traversal"))?;
                    self.parents[last] = None;
                    out.defect("drawio.parentCycle", "Cyclic parent relation detached in source order; original reference retained".into(), &loc[last])?;
                    break;
                }
                colors[n] = 1;
                path.push(n);
                at = self.parents[n];
            }
            for &n in path.iter().rev() {
                self.depths[n] = self.parents[n].map_or(0, |p| self.depths[p] + 1);
                if self.depths[n] > usize::from(out.options.limits.max_nesting_depth) {
                    return Err(limit(
                        "max_nesting_depth",
                        "Drawio group hierarchy exceeds request limit",
                    ));
                }
                colors[n] = 2;
            }
        }
        Ok(())
    }
}
