use crate::geometry;
use crate::paragraph_list;
use crate::resource_association;
use crate::{SemanticNode, SemanticSnapshot, SourceBoundary};
use into_markdown_core::{Asset, Block, BlockNode, ConversionError, ExecutionContext};
use std::collections::BTreeSet;

#[derive(Clone, Default)]
struct ContainerBoundary {
    page: Option<u32>,
    slide: Option<u32>,
    sheet: Option<String>,
}

pub(crate) fn snapshot(
    document: &into_markdown_core::Document,
    assets: &[Asset],
    context: &ExecutionContext,
) -> Result<SemanticSnapshot, ConversionError> {
    let mut state =
        State { context, nodes: Vec::new(), referenced_assets: BTreeSet::new(), work: 0 };
    state.visit_siblings(&document.blocks, None, 0, &ContainerBoundary::default())?;
    state.referenced_assets.extend(resource_association::associate_named_attachments(
        &mut state.nodes,
        assets,
        context,
    )?);
    let assets = resource_association::assets(assets, &state.referenced_assets, context)?;
    Ok(SemanticSnapshot { nodes: state.nodes, assets })
}

struct State<'a> {
    context: &'a ExecutionContext,
    nodes: Vec<SemanticNode>,
    referenced_assets: BTreeSet<String>,
    work: u64,
}

impl State<'_> {
    fn visit_siblings(
        &mut self,
        nodes: &[BlockNode],
        parent_id: Option<&str>,
        depth: u16,
        inherited: &ContainerBoundary,
    ) -> Result<(), ConversionError> {
        if depth > self.context.resource_limits().max_nesting_depth {
            return Err(ConversionError::ResourceLimit {
                limit: "max_nesting_depth",
                detail: format!(
                    "semantic layout depth {depth} > {}",
                    self.context.resource_limits().max_nesting_depth
                ),
            });
        }
        for (sibling_order, node) in nodes.iter().enumerate() {
            self.visit(
                node,
                parent_id,
                u64::try_from(sibling_order).map_err(|_| work_limit("sibling order"))?,
                depth,
                inherited,
            )?;
        }
        Ok(())
    }

    fn visit(
        &mut self,
        node: &BlockNode,
        parent_id: Option<&str>,
        sibling_order: u64,
        depth: u16,
        inherited: &ContainerBoundary,
    ) -> Result<(), ConversionError> {
        self.context.checkpoint()?;
        self.work = self.work.checked_add(1).ok_or_else(|| work_limit("node count"))?;
        if self.work > self.context.resource_limits().max_table_cells {
            return Err(ConversionError::ResourceLimit {
                limit: "semantic_layout_work",
                detail: format!(
                    "{} nodes > {} work units",
                    self.work,
                    self.context.resource_limits().max_table_cells
                ),
            });
        }
        let mut boundary = geometry::boundary(&node.provenance.locator);
        inherit_boundary(&mut boundary, inherited);
        match &node.block {
            Block::Page { number, .. } => boundary.page = Some(*number),
            Block::Slide { number, .. } => boundary.slide = Some(*number),
            Block::Sheet { name, .. } => boundary.sheet = Some(name.clone()),
            _ => {}
        }
        let references = resource_association::references(&node.block, self.context)?;
        self.referenced_assets.extend(
            references
                .iter()
                .filter(|reference| reference.kind == "asset")
                .map(|reference| reference.target.clone()),
        );
        let table = match &node.block {
            Block::Table { rows, .. } => Some(crate::table::topology(rows, self.context)?),
            _ => None,
        };
        let order = u64::try_from(self.nodes.len()).map_err(|_| work_limit("reading order"))?;
        self.nodes.push(SemanticNode {
            id: node.id.0.clone(),
            kind: paragraph_list::kind(&node.block),
            parent_id: parent_id.map(str::to_owned),
            order,
            sibling_order,
            depth,
            text: paragraph_list::text(&node.block, self.context)?,
            boundary,
            bounds: geometry::normalize(node.provenance.locator.bounds)?,
            source_chain: resource_association::source_chain(
                &node.block,
                &node.provenance,
                self.context,
            )?,
            table,
            references,
        });

        let child_boundary = container_boundary(&self.nodes.last().expect("just pushed").boundary);
        let child_depth = depth.checked_add(1).ok_or_else(|| work_limit("nesting depth"))?;
        let node_id = node.id.0.as_str();
        match &node.block {
            Block::List { items, .. } => {
                let mut sibling = 0_u64;
                for item in items {
                    for child in &item.blocks {
                        self.visit(child, Some(node_id), sibling, child_depth, &child_boundary)?;
                        sibling = sibling.checked_add(1).ok_or_else(|| work_limit("list order"))?;
                    }
                }
            }
            Block::Table { rows, .. } => {
                let mut sibling = 0_u64;
                for row in rows {
                    for cell in &row.cells {
                        for child in &cell.blocks {
                            self.visit(
                                child,
                                Some(node_id),
                                sibling,
                                child_depth,
                                &child_boundary,
                            )?;
                            sibling = sibling
                                .checked_add(1)
                                .ok_or_else(|| work_limit("table child order"))?;
                        }
                    }
                }
            }
            Block::Footnote { blocks, .. }
            | Block::Page { blocks, .. }
            | Block::Slide { blocks, .. }
            | Block::Sheet { blocks, .. } => {
                self.visit_siblings(blocks, Some(node_id), child_depth, &child_boundary)?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn inherit_boundary(boundary: &mut SourceBoundary, inherited: &ContainerBoundary) {
    if boundary.page.is_none() {
        boundary.page = inherited.page;
    }
    if boundary.slide.is_none() {
        boundary.slide = inherited.slide;
    }
    if boundary.sheet.is_none() {
        boundary.sheet.clone_from(&inherited.sheet);
    }
}

fn container_boundary(boundary: &SourceBoundary) -> ContainerBoundary {
    ContainerBoundary { page: boundary.page, slide: boundary.slide, sheet: boundary.sheet.clone() }
}

fn work_limit(detail: &'static str) -> ConversionError {
    ConversionError::ResourceLimit { limit: "semantic_layout_work", detail: detail.into() }
}
