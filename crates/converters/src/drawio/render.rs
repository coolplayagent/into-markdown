use super::{
    budget::limit,
    graph::Graph,
    labels,
    model::{Cell, Model},
    output::Output,
    pages::Page,
};
use into_markdown_core::{
    Block, BlockNode, Cell as TableCell, ConversionError, ListItem, ListKind, SourceLocator,
    TableRow,
};

struct PageView<'a> {
    cells: &'a [Cell],
    graph: Graph<'a>,
    locations: Vec<SourceLocator>,
    number: u32,
}

pub(super) fn render(
    model: &Model,
    page: &Page,
    number: u32,
    out: &mut Output<'_>,
) -> Result<Vec<BlockNode>, ConversionError> {
    let _locations_memory = out.context.reserve_memory(model.cells.len() as u64 * 512)?;
    let locations: Vec<_> = model
        .cells
        .iter()
        .enumerate()
        .map(|(i, cell)| {
            cell.locator(number, i, page.model.as_ref().map(|r| r.start), &page.payload_span)
        })
        .collect();
    let graph = Graph::new(&model.cells, &locations, out)?;
    let view = PageView { cells: &model.cells, graph, locations, number };
    let loc = page_locator(page, number);
    let title = if page.name.is_empty() {
        format!("Page {number}")
    } else {
        format!("Page {number}: {}", page.name)
    };
    let mut blocks = vec![out.heading(1, &title, &loc)?];
    if !page.id.is_empty() {
        blocks.push(out.paragraph(&format!("Page ID: {}", page.id), &loc)?);
    }
    let mut items = Vec::new();
    for i in 0..view.cells.len() {
        if view.graph.parents[i].is_none() && !view.cells[i].edge() {
            view.item(i, 0, &mut items, out)?;
        }
    }
    if !items.is_empty() {
        blocks.push(out.heading(2, "Nodes", &loc)?);
        blocks.push(out.block(Block::List { kind: ListKind::Bullet, start: 1, items }, &loc)?);
    }
    if view.cells.iter().any(Cell::edge) {
        blocks.push(out.heading(2, "Connections", &loc)?);
        blocks.push(view.connections(&loc, out)?);
    }
    if blocks.len() == 1 {
        blocks.push(out.paragraph("Empty diagram", &loc)?);
    }
    Ok(blocks)
}

pub(super) fn page_locator(page: &Page, number: u32) -> SourceLocator {
    SourceLocator {
        page: Some(number),
        part: Some(format!("drawio/pages/{number}")),
        byte_start: Some(page.span.start as u64),
        byte_end: Some(page.span.end as u64),
        ..SourceLocator::default()
    }
}

impl PageView<'_> {
    fn identity(&self, i: usize, out: &mut Output<'_>) -> Result<String, ConversionError> {
        let id = self.cells[i].id();
        out.charge(id.len() + 80)?;
        Ok(format!("{} [p{}:c{}]", if id.is_empty() { "unnamed" } else { id }, self.number, i + 1))
    }

    fn item(
        &self,
        i: usize,
        depth: usize,
        items: &mut Vec<ListItem>,
        out: &mut Output<'_>,
    ) -> Result<(), ConversionError> {
        out.context.checkpoint()?;
        let cell = &self.cells[i];
        if cell.edge() {
            return Ok(());
        }
        // The model root is bookkeeping; named roots remain visible.
        if cell.id() == "0"
            && cell.attr("parent").is_empty()
            && cell.attr("vertex") != "1"
            && cell.label().is_empty()
        {
            for &child in &self.graph.children[i] {
                self.item(child, depth, items, out)?;
            }
            return Ok(());
        }
        if depth >= 6 {
            return self.flat_items(i, items, out);
        }
        let mut blocks = vec![self.node(i, false, out)?];
        let mut nested = Vec::new();
        for &child in &self.graph.children[i] {
            self.item(child, depth + 1, &mut nested, out)?;
        }
        if !nested.is_empty() {
            blocks.push(out.block(
                Block::List { kind: ListKind::Bullet, start: 1, items: nested },
                &self.locations[i],
            )?);
        }
        out.charge(256)?;
        items.push(ListItem { checked: None, marker_label: None, blocks });
        Ok(())
    }

    fn flat_items(
        &self,
        root: usize,
        items: &mut Vec<ListItem>,
        out: &mut Output<'_>,
    ) -> Result<(), ConversionError> {
        out.warning(
            "drawio.hierarchyExpanded",
            "Deep group hierarchy represented by full ancestor paths within the IR depth limit"
                .into(),
            &self.locations[root],
        )?;
        let _stack_memory = out.context.reserve_memory(self.cells.len() as u64 * 16)?;
        let mut stack = vec![root];
        while let Some(i) = stack.pop() {
            out.context.checkpoint()?;
            if self.cells[i].edge() {
                continue;
            }
            let node = self.node(i, true, out)?;
            out.charge(256)?;
            items.push(ListItem { checked: None, marker_label: None, blocks: vec![node] });
            stack.extend(self.graph.children[i].iter().rev().copied());
        }
        Ok(())
    }

    fn node(
        &self,
        i: usize,
        expanded: bool,
        out: &mut Output<'_>,
    ) -> Result<BlockNode, ConversionError> {
        let cell = &self.cells[i];
        let loc = &self.locations[i];
        let mut content = labels::label(cell, loc, out)?;
        let id = self.identity(i, out)?;
        content.push(out.inline(&format!(" — {id}"))?);
        if !cell.attr("parent").is_empty() {
            let parent = self.parent_reference(i, out)?;
            content.push(out.inline(&parent)?);
        }
        if cell.attr("vertex") != "1" {
            content.push(out.inline("; layer")?);
        } else if !self.graph.children[i].is_empty()
            || cell.style("group").is_some()
            || cell.style("swimlane").is_some()
        {
            content.push(out.inline("; group")?);
        }
        if expanded {
            let mut parents = Vec::new();
            let mut at = self.graph.parents[i];
            while let Some(parent) = at {
                out.charge(32)?;
                parents.push(parent);
                at = self.graph.parents[parent];
            }
            content.push(out.inline("; ancestors: ")?);
            for p in parents.into_iter().rev() {
                let identity = self.identity(p, out)?;
                content.push(out.inline(&identity)?);
                content.push(out.inline(" / ")?);
            }
        }
        out.block(Block::Paragraph(content), loc)
    }

    fn connections(
        &self,
        loc: &SourceLocator,
        out: &mut Output<'_>,
    ) -> Result<BlockNode, ConversionError> {
        let count = self.cells.iter().filter(|c| c.edge()).count() + 1;
        if count as u64 > out.options.limits.max_table_rows {
            return Err(limit("max_table_rows", "Drawio connection table exceeds row limit"));
        }
        if out.options.limits.max_table_columns < 5 {
            return Err(limit(
                "max_table_columns",
                "Drawio connection table requires five columns",
            ));
        }
        if count as u64 * 5 > out.options.limits.max_table_cells {
            return Err(limit("max_table_cells", "Drawio connection table exceeds cell limit"));
        }
        out.charge(count * 512)?;
        let mut rows = Vec::new();
        let mut header = Vec::new();
        for name in ["Connection", "Source", "Target", "Direction", "Label"] {
            header.push(TableCell {
                row_span: 1,
                column_span: 1,
                header: true,
                blocks: vec![out.paragraph(name, loc)?],
            });
        }
        rows.push(TableRow { cells: header });
        for (i, cell) in self.cells.iter().enumerate().filter(|(_, c)| c.edge()) {
            let loc = &self.locations[i];
            let mut id = self.identity(i, out)?;
            id.push_str(&self.parent_reference(i, out)?);
            let source = self.endpoint(i, "source", out)?;
            let target = self.endpoint(i, "target", out)?;
            let mut row = Vec::new();
            for text in [&id, &source, &target, direction(cell)] {
                row.push(TableCell {
                    row_span: 1,
                    column_span: 1,
                    header: false,
                    blocks: vec![out.paragraph(text, loc)?],
                });
            }
            let content = labels::label(cell, loc, out)?;
            let mut labels = vec![out.block(Block::Paragraph(content), loc)?];
            for &child in &self.graph.labels[i] {
                let mut content = labels::label(&self.cells[child], &self.locations[child], out)?;
                let identity = self.identity(child, out)?;
                content.push(out.inline(&format!(" — {identity}"))?);
                let parent = self.parent_reference(child, out)?;
                content.push(out.inline(&parent)?);
                labels.push(out.block(Block::Paragraph(content), &self.locations[child])?);
            }
            row.push(TableCell { row_span: 1, column_span: 1, header: false, blocks: labels });
            rows.push(TableRow { cells: row });
        }
        out.block(Block::Table { rows, alignments: Vec::new() }, loc)
    }

    fn parent_reference(&self, i: usize, out: &mut Output<'_>) -> Result<String, ConversionError> {
        let parent = self.cells[i].attr("parent");
        if parent.is_empty() {
            return Ok(String::new());
        }
        out.charge(parent.len() * 2 + 64)?;
        let mut text = format!("; parent: {parent}");
        if let Some(matches) = self.graph.ids.get(parent).filter(|v| v.len() > 1) {
            text.push_str("; candidates: ");
            for &candidate in matches {
                let identity = self.identity(candidate, out)?;
                out.charge(identity.len() * 2 + 8)?;
                text.push_str(&identity);
                text.push_str("; ");
            }
        }
        Ok(text)
    }

    fn endpoint(
        &self,
        i: usize,
        key: &str,
        out: &mut Output<'_>,
    ) -> Result<String, ConversionError> {
        let cell = &self.cells[i];
        let id = cell.attr(key);
        if id.is_empty() {
            let point = if key == "source" { &cell.source_point } else { &cell.target_point };
            return Ok(format!(
                "free endpoint {}",
                point.as_deref().unwrap_or("(position unspecified)")
            ));
        }
        out.charge(id.len() + 80)?;
        match self.graph.ids.get(id) {
            None => Ok(format!("{id} [missing]")),
            Some(matches) => {
                let mut text = String::new();
                for &candidate in matches {
                    let identity = self.identity(candidate, out)?;
                    out.charge(identity.len() * 2 + 8)?;
                    if !text.is_empty() {
                        text.push_str(" | ");
                    }
                    text.push_str(&identity);
                }
                Ok(text)
            }
        }
    }
}

fn direction(cell: &Cell) -> &'static str {
    let arrow = |value: Option<&str>, default: bool| {
        value.map_or(default, |v| !matches!(v, "none" | "0" | ""))
    };
    match (arrow(cell.style("startArrow"), false), arrow(cell.style("endArrow"), true)) {
        (false, true) => "source → target",
        (true, false) => "source ← target",
        (true, true) => "source ↔ target",
        (false, false) => "source — target",
    }
}
