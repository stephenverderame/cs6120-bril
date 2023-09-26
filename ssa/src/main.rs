use std::collections::HashMap;

use bril_rs::{Code, Function, Instruction, Type, ValueOps};
use cfg::{
    analysis::{
        analyze,
        defined_vars::{self, DefinedVars},
        dominators,
        live_vars::{self, LiveVars},
        AnalysisResult,
    },
    CfgNode, CFG_START_ID,
};
use common_cli::{cli_args, compiler_pass};

#[cli_args]
struct ExtraArgs {
    /// Pass this flag to transform out of SSA form instead of transforming
    /// into SSA form
    #[arg(long, short, default_value_t = false)]
    out: bool,
}

/// Invokes global dead code elimination on the cfg
#[compiler_pass(all_labels)]
fn ssa(cfg: Cfg, args: &CLIArgs, f: &Function) -> Cfg {
    (if args.out {
        cfg
    } else {
        let dom_tree = dominators::compute_dominators(&cfg);
        let base_names = f
            .args
            .iter()
            .map(|x| (x.name.clone(), x.name.clone()))
            .collect();
        rename_vars(
            add_phi_nodes(cfg, f, &dom_tree),
            CFG_START_ID,
            HashMap::new(),
            &mut HashMap::new(),
            base_names,
            &dom_tree,
        )
    })
    .clean()
}

/// Inserts a phi node as the first instruction of a block if one with the
/// same destination does not already exist. If one does exist, it will be
/// replaced with the new phi node
/// # Arguments
/// * `cfg` - The cfg
/// * `block` - The block to insert the phi node into
/// * `phi` - The phi node to insert
/// # Returns
/// * The cfg with the phi node inserted
fn add_phi_to_block(
    mut cfg: Cfg,
    block: usize,
    phi: Instruction,
    live_vars: &mut AnalysisResult<LiveVars>,
    defined_vars: &mut AnalysisResult<DefinedVars>,
) -> Cfg {
    if let Some(CfgNode::Block(block)) = cfg.blocks.get_mut(&block) {
        if let Some(existing_pos) = block
            .instrs
            .iter()
            .position(|(_, instr)| instr.get_dest() == phi.get_dest())
        {
            block.instrs[existing_pos].1 = phi;
        } else {
            let prev_block_starter = block
                .instrs
                .iter()
                .chain(block.terminator.as_ref())
                .next()
                .unwrap()
                .0;
            block.instrs.insert(0, (cfg.last_instr_id, phi));
            live_vars.duplicate_facts(prev_block_starter, cfg.last_instr_id);
            defined_vars.duplicate_facts(prev_block_starter, cfg.last_instr_id);
            cfg.last_instr_id += 1;
        }
    }
    cfg
}

/// Gets the labels of predecessor blocks of `frontier_blk` that have `var` live out
/// # Arguments
/// * `preds` - Inverse cfg adjacency list
/// * `live_vars` - The live variables analysis result
/// * `cfg` - The cfg
/// * `var` - The defined variable that an SSA node is being constructed for
/// * `frontier_blk` - The block in the dominance frontier of the definition that
/// we are constructing an phi node for
/// # Returns
/// * The labels of the predecessor blocks that have `var` live out (and therefore)
/// need to be merged in a phi node
fn get_ssa_pred_labels(
    preds: &HashMap<usize, Vec<usize>>,
    live_vars: &AnalysisResult<LiveVars>,
    defined_vars: &AnalysisResult<DefinedVars>,
    cfg: &Cfg,
    var: &str,
    frontier_blk: usize,
) -> Vec<String> {
    preds[&frontier_blk]
        .iter()
        .filter_map(|blk| {
            if let CfgNode::Block(block) = &cfg.blocks[blk] {
                if live_vars.block_facts(block, *blk).0.is_live_out(var)
                    && defined_vars
                        .block_facts(block, *blk)
                        .1
                        .iter()
                        .any(|x| x.is_defined(var))
                {
                    Some(format!("block.{blk}"))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect()
}

/// Adds phi nodes to all blocks in the dominance frontier of a definition
/// The phi nodes will be inserted as the first instructions of the block
/// and will use the same variable names as the definition
/// # Arguments
/// * `cfg` - The cfg
/// * `f` - The function
/// # Returns
/// * The cfg with phi nodes inserted
fn add_phi_nodes(
    mut cfg: Cfg,
    f: &Function,
    dom_tree: &dominators::DomTree,
) -> Cfg {
    let mut vars = find_vars(&cfg, f);
    let mut added_phi_nodes: HashMap<(String, usize), Vec<String>> =
        HashMap::new();
    let preds = cfg.preds();
    let mut changed = true;
    let empty_preds = vec![];
    let mut live_vars = analyze(&cfg, &live_vars::LiveVars::top(), None);
    let mut defined_vars =
        analyze(&cfg, &defined_vars::DefinedVars::top(f), None);
    while changed {
        changed = false;
        for (var, def_blocks) in vars.iter_mut() {
            for (def_blk, def_type) in def_blocks {
                for frontier_blk in dom_tree.dom_frontier(*def_blk, &cfg) {
                    let existing_phi = added_phi_nodes
                        .get(&(var.to_string(), frontier_blk))
                        .unwrap_or(&empty_preds);
                    let pred_labels = get_ssa_pred_labels(
                        &preds,
                        &live_vars,
                        &defined_vars,
                        &cfg,
                        var,
                        frontier_blk,
                    );
                    if existing_phi != &pred_labels {
                        added_phi_nodes.insert(
                            (var.to_string(), frontier_blk),
                            pred_labels.clone(),
                        );
                        let phi = bril_rs::Instruction::Value {
                            op: bril_rs::ValueOps::Phi,
                            args: pred_labels
                                .iter()
                                .map(|_| var.to_string())
                                .collect(),
                            dest: var.to_string(),
                            funcs: vec![],
                            labels: pred_labels,
                            pos: None,
                            op_type: def_type.clone(),
                        };
                        cfg = add_phi_to_block(
                            cfg,
                            frontier_blk,
                            phi,
                            &mut live_vars,
                            &mut defined_vars,
                        );
                        changed = true;
                    }
                }
            }
        }
    }
    cfg
}

/// Renames variables in a block to be unique
/// # Arguments
/// * `cfg` - The cfg
/// * `block_id` - The block to rename variables in
/// * `cur_names` - A map from variable name to the current index of that
/// variable name. The latest variable name will be `var.{cur_index}`
/// if a mapping is present, otherwise it will be `var`
/// * `latest_names` - A map from variable name to the globally latest index of that
/// * `last_block_id` - The id of the block that was last visited and is calling
/// this function
/// * `base_names` - A map from variable name to the original name of the
/// variable.
fn rename_vars(
    mut cfg: Cfg,
    block_id: usize,
    mut cur_names: HashMap<String, u64>,
    latest_names: &mut HashMap<String, u64>,
    mut base_names: HashMap<String, String>,
    dom_tree: &dominators::DomTree,
) -> Cfg {
    if let CfgNode::Block(block) = cfg.blocks.get_mut(&block_id).unwrap() {
        for (_, instr) in
            block.instrs.iter_mut().chain(block.terminator.as_mut())
        {
            (base_names, cur_names) =
                rename_instr(instr, base_names, cur_names, latest_names);
        }
    }
    for succ in cfg.adj_lst[&block_id].nodes() {
        base_names =
            rename_phi_args(&mut cfg, succ, block_id, base_names, &cur_names);
    }
    for nxt in dom_tree.immediately_dominated(block_id) {
        cfg = rename_vars(
            cfg,
            nxt,
            cur_names.clone(),
            latest_names,
            base_names.clone(),
            dom_tree,
        );
    }
    cfg
}

/// Renames variables in an instruction to be unique
/// # Arguments
/// * `instr` - The instruction to rename variables in
/// * `last_block_id` - The id of the block that was last visited and is calling
/// this function
/// * `base_names` - A map from variable name to the original name of the
/// variable.
/// * `cur_names` - A map from variable name to the current index of that
/// variable name. The latest variable name will be `var.{cur_index}`
/// if a mapping is present, otherwise it will be `var`
/// * `latest_names` - A map from variable name to the globally latest index of that
/// variable name. The latest variable name will be `var.{latest_index}`
/// # Returns
/// * A tuple of the updated `base_names` and `cur_names`
fn rename_instr(
    instr: &mut Instruction,
    mut base_names: HashMap<String, String>,
    mut cur_names: HashMap<String, u64>,
    latest_names: &mut HashMap<String, u64>,
) -> (HashMap<String, String>, HashMap<String, u64>) {
    // handle args
    if let Instruction::Value {
        op: ValueOps::Phi, ..
    } = instr
    {
        // phi nodes handled separately
    } else if let Some(args) = instr.get_args_mut() {
        for arg in args {
            let base_arg = base_names.get(arg).unwrap_or(arg).clone();
            base_names.remove(arg);
            let new_suffix = cur_names
                .get(&base_arg)
                .copied()
                .map(|x| format!(".{x}"))
                .unwrap_or_default();
            *arg = format!("{base_arg}{new_suffix}");
            base_names.insert(arg.clone(), base_arg);
        }
    }
    // handle dest
    if let Some(dest) = instr.get_dest_mut() {
        let base_dest = base_names.get(dest).unwrap_or(dest).clone();
        base_names.remove(dest);
        let new_name_suffix =
            latest_names.entry(base_dest.clone()).or_default();
        *dest = format!("{base_dest}.{new_name_suffix}");
        *cur_names
            .entry(base_dest.clone())
            .or_insert(*new_name_suffix) = *new_name_suffix;
        *new_name_suffix += 1;
        base_names.insert(dest.clone(), base_dest);
    }
    (base_names, cur_names)
}

/// Renames the arguments of phi nodes in a block to be unique
/// # Arguments
/// * `cfg` - The cfg
/// * `block_id` - The block to rename variables in
/// * `pred_block_id` - The id of the predecessor block that was last visited
/// * `base_names` - A map from variable name to the original name of the
/// variable.
/// * `latest_names` - A map from variable name to the latest index of that
/// variable name. The latest variable name will be `var.{latest_index}`
/// if a mapping is present, otherwise it will be `var`
/// # Returns
/// * A tuple of the updated `base_names` and `latest_names`
fn rename_phi_args(
    cfg: &mut Cfg,
    block_id: usize,
    pred_block_id: usize,
    mut base_names: HashMap<String, String>,
    latest_names: &HashMap<String, u64>,
) -> HashMap<String, String> {
    if let CfgNode::Block(block) = cfg.blocks.get_mut(&block_id).unwrap() {
        for (_, instr) in block.instrs.iter_mut() {
            // handle phi nodes -> only replace argument with the last block as
            // the corresponding label
            if let Instruction::Value {
                op: ValueOps::Phi,
                labels,
                args,
                ..
            } = instr
            {
                let pos = labels
                    .iter()
                    .position(|x| x == &format!("block.{pred_block_id}"));
                if let Some(pos) = pos {
                    let base_name = base_names
                        .get(&args[pos])
                        .unwrap_or(&args[pos])
                        .clone();
                    base_names.remove(&args[pos]);
                    args[pos] = format!(
                        "{base_name}{}",
                        latest_names
                            .get(&base_name)
                            .copied()
                            .map(|x| format!(".{x}"))
                            .unwrap_or_default()
                    );
                    base_names.insert(args[pos].clone(), base_name.to_string());
                }
            }
        }
    }
    base_names
}

/// Gets a map from variable name to the blocks that define it
/// Function arguments are considered to be defined in the ghost start block
/// # Arguments
/// * `cfg` - The cfg
/// * `f` - The function
/// # Returns
/// * A map from variable name to a vector of tuples of (block id, type)
fn find_vars(
    cfg: &Cfg,
    f: &bril_rs::Function,
) -> HashMap<String, Vec<(usize, Type)>> {
    let mut vars: HashMap<_, _> = f
        .args
        .iter()
        .map(|x| (x.name.clone(), vec![(CFG_START_ID, x.arg_type.clone())]))
        .collect();
    for (blk_id, block) in &cfg.blocks {
        if let CfgNode::Block(block) = block {
            for (_, instr) in block.instrs.iter() {
                if let (Some(dest), Some(typ)) =
                    (instr.get_dest(), instr.get_type())
                {
                    vars.entry(dest).or_default().push((*blk_id, typ));
                }
            }
        }
    }
    vars
}

fn ssa_post(instrs: Vec<Code>) -> Vec<Code> {
    instrs
}
