use std::io::Write;

use super::SpecCommand;
use crate::{config::ConfigPaths, specs::SpecRegistry, terminal::RiskLevel};

pub fn run(output: &mut dyn Write, command: SpecCommand) -> crate::Result<()> {
    let paths = ConfigPaths::discover()?;
    let registry = SpecRegistry::load(Some(&paths.specs_directory));
    match command {
        SpecCommand::List => list(output, &registry)?,
        SpecCommand::Show { name } => show(output, &registry, &name)?,
        SpecCommand::Validate => validate(output, &registry)?,
    }
    Ok(())
}

fn list(output: &mut dyn Write, registry: &SpecRegistry) -> crate::Result<()> {
    for command in registry.commands() {
        writeln!(
            output,
            "{}\t{}\t{}",
            command.name,
            risk_name(command.risk),
            command.description
        )?;
    }
    write_diagnostics(output, registry)?;
    Ok(())
}

fn show(output: &mut dyn Write, registry: &SpecRegistry, name: &str) -> crate::Result<()> {
    let command = registry
        .get(name)
        .or_else(|| registry.commands().find(|candidate| candidate.id == name));
    let command = command.ok_or_else(|| {
        crate::Error::Config(format!("no enabled command specification for {name:?}"))
    })?;
    writeln!(output, "id: {}", command.id)?;
    writeln!(output, "name: {}", command.name)?;
    if !command.aliases.is_empty() {
        writeln!(output, "aliases: {}", command.aliases.join(", "))?;
    }
    writeln!(output, "description: {}", command.description)?;
    writeln!(output, "requires arguments: {}", command.requires_arguments)?;
    writeln!(output, "risk: {}", risk_name(command.risk))?;
    writeln!(output, "default: {}", command.default)?;
    writeln!(output, "source: {}", command.provenance.display())?;
    for recipe in &command.recipes {
        writeln!(
            output,
            "recipe {}: {} [{}] - {}",
            recipe.id,
            recipe.template,
            risk_name(recipe.risk),
            recipe.description
        )?;
    }
    Ok(())
}

fn validate(output: &mut dyn Write, registry: &SpecRegistry) -> crate::Result<()> {
    write_diagnostics(output, registry)?;
    if !registry.diagnostics().is_empty() {
        return Err(crate::Error::Config(format!(
            "{} command specification error(s)",
            registry.diagnostics().len()
        )));
    }
    writeln!(
        output,
        "valid: {} enabled command specifications",
        registry.commands().count()
    )?;
    Ok(())
}

fn write_diagnostics(output: &mut dyn Write, registry: &SpecRegistry) -> crate::Result<()> {
    for diagnostic in registry.diagnostics() {
        writeln!(
            output,
            "{} {}: {}",
            diagnostic.code,
            diagnostic.path.display(),
            diagnostic.message
        )?;
    }
    Ok(())
}

const fn risk_name(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::ReadOnly => "read_only",
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
        RiskLevel::Unknown => "unknown",
    }
}
