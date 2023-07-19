use crate::{AgentModel, BoardState};
use atomic_float::AtomicF32;
use bitvec::vec::BitVec;
use environment::{Environment, GameStatus, Stone};
use mcts::{Node, NodePtr, State, MCTS};
use parking_lot::RwLock;
use rand::prelude::*;
use rayon::{prelude::*, ThreadPool, ThreadPoolBuilder};
use std::sync::atomic::Ordering;
use tensorflow::{Session, SessionRunArgs, Status, Tensor};

pub struct ParallelMCTSExecutor {
    thread_pool: ThreadPool,
}

impl ParallelMCTSExecutor {
    pub const C_PUCT: f32 = 1.0;

    pub fn new() -> Self {
        Self {
            thread_pool: ThreadPoolBuilder::new().build().unwrap(),
        }
    }

    pub fn execute(
        &self,
        count: usize,
        batch_size: usize,
        session: &Session,
        agent: &AgentModel,
        mcts_list: &[MCTS<BoardState>],
    ) -> Result<(), Status> {
        self.thread_pool.install(|| {
            let mut processed_count = 0;

            loop {
                if count <= processed_count {
                    break;
                }

                processed_count += batch_size;

                let requests = mcts_list
                    .par_iter()
                    .flat_map(|mcts| {
                        let mut rng = thread_rng();
                        let mut requests = Vec::with_capacity(batch_size);

                        for _ in 0..batch_size {
                            let node = mcts.select_leaf(|parent, children| {
                                let parent_n = parent.n.load(Ordering::Relaxed);
                                children
                                    .iter()
                                    .map(|child| compute_ucb_1(parent_n, child, Self::C_PUCT))
                                    .enumerate()
                                    .max_by(|(_, a), (_, b)| f32::total_cmp(a, b))
                                    .unwrap()
                                    .0
                            });

                            if node.state.is_terminal() {
                                // If the leaf node is terminal state, we don't need to expand it.
                                // Instead we perform backup from the leaf node.
                                node.propagate(node.state.z.load(Ordering::Relaxed));
                                node.v_loss.fetch_sub(1, Ordering::Relaxed);
                                continue;
                            }

                            // Select any possible action.
                            // Since the leaf node doesn't have terminal state, we need to expand it.
                            let action = {
                                let mut bits = BitVec::<usize>::repeat(
                                    false,
                                    Environment::BOARD_SIZE * Environment::BOARD_SIZE,
                                );

                                for children in node.children.read().iter() {
                                    bits.set(children.action.unwrap(), true);
                                }

                                let available_actions = (0..Environment::BOARD_SIZE
                                    * Environment::BOARD_SIZE)
                                    .filter(|&action| {
                                        node.state.is_available_action(action) && !bits[action]
                                    })
                                    .collect::<Vec<_>>();
                                available_actions.choose(&mut rng).cloned()
                            };
                            let action = if let Some(action) = action {
                                action
                            } else {
                                // There's no action for now.
                                // Note that this not means the game is over.
                                node.v_loss.fetch_sub(1, Ordering::Relaxed);
                                continue;
                            };

                            // Place the stone.
                            let mut env = node.state.env.clone();
                            let status = env.place_stone(action).unwrap();
                            let terminal_reward = match status {
                                GameStatus::InProgress => None,
                                GameStatus::Draw => Some(0f32),
                                GameStatus::BlackWin => Some(1f32),
                                GameStatus::WhiteWin => Some(1f32),
                            };

                            // Pre-compute policy.
                            // This will be overwritten by the neural network evaluation.
                            // Until then, we use the uniform distribution.
                            let mut policy =
                                [1f32; Environment::BOARD_SIZE * Environment::BOARD_SIZE];

                            for action in 0..Environment::BOARD_SIZE * Environment::BOARD_SIZE {
                                if env.board[action] != Stone::Empty {
                                    policy[action] = 0f32;
                                }
                            }

                            let sum = policy.iter().sum::<f32>();
                            if f32::EPSILON <= sum {
                                for policy in policy.iter_mut() {
                                    *policy /= sum;
                                }
                            }

                            // Pre-expand the node.
                            let expanded_child = match mcts.expand(
                                node,
                                action,
                                BoardState {
                                    env,
                                    status,
                                    policy: RwLock::new(policy),
                                    z: AtomicF32::new(terminal_reward.unwrap_or(0f32)),
                                },
                            ) {
                                Some(child) => {
                                    node.v_loss.fetch_sub(1, Ordering::Relaxed);
                                    child
                                }
                                None => {
                                    // The node is already expanded by other thread.
                                    // We don't need to expand it again.
                                    node.v_loss.fetch_sub(1, Ordering::Relaxed);
                                    continue;
                                }
                            };

                            // Collect the requests.
                            requests.push(NNEvalRequest {
                                node: expanded_child,
                                terminal_reward,
                            });
                        }

                        requests
                    })
                    .collect::<Vec<_>>();

                if requests.is_empty() {
                    // There's no request for now.
                    // It can be happened when all leaf nodes are terminal state.
                    continue;
                }

                // Prepare the input tensor.
                let mut input_tensor = Tensor::new(&[
                    requests.len() as u64,
                    Environment::BOARD_SIZE as u64,
                    Environment::BOARD_SIZE as u64,
                    2,
                ]);

                // Encode the board state.
                for (batch_index, request) in requests.iter().enumerate() {
                    let env = &request.node.state.env;
                    env.encode_board(
                        env.turn.opponent(),
                        &mut input_tensor[batch_index
                            * Environment::BOARD_SIZE
                            * Environment::BOARD_SIZE
                            * 2
                            ..(batch_index + 1)
                                * Environment::BOARD_SIZE
                                * Environment::BOARD_SIZE
                                * 2],
                    );
                }

                // Prepare the evaluation run arguments.
                let mut eval_run_args = SessionRunArgs::new();
                eval_run_args.add_feed(&agent.op_input, 0, &input_tensor);
                eval_run_args.add_target(&agent.op_p_output);
                eval_run_args.add_target(&agent.op_v_output);

                let p_fetch_token = eval_run_args.request_fetch(&agent.op_p_output, 0);
                let v_fetch_token = eval_run_args.request_fetch(&agent.op_v_output, 0);

                // Evaluate the network.
                session.run(&mut eval_run_args)?;

                let p = eval_run_args.fetch::<f32>(p_fetch_token)?;
                let v = eval_run_args.fetch::<f32>(v_fetch_token)?;

                for (batch_index, request) in requests.iter().enumerate() {
                    let node = &*request.node;
                    let p = &p[batch_index * (Environment::BOARD_SIZE * Environment::BOARD_SIZE)
                        ..(batch_index + 1) * (Environment::BOARD_SIZE * Environment::BOARD_SIZE)];
                    let v = v[batch_index];
                    let reward = request.terminal_reward.unwrap_or_else(|| v);

                    // Update the pre-expanded child node.
                    node.state.policy.write().copy_from_slice(p);

                    // Update children's prior probability.
                    // This is required because every node after expanded are holding dummy prior probabilities.
                    for child in node.children.read().iter() {
                        let child = &*child;
                        let action = child.action.unwrap();
                        let p = p[action];
                        child.p.store(p, Ordering::Relaxed);
                    }

                    // Perform backup from the expanded child node.
                    node.propagate(reward);
                }
            }

            Ok(())
        })
    }
}

struct NNEvalRequest {
    pub node: NodePtr<BoardState>,
    pub terminal_reward: Option<f32>,
}

fn compute_ucb_1<S>(parent_n: u64, node: &Node<S>, c: f32) -> f32
where
    S: State,
{
    let n = node.n.load(Ordering::Relaxed);
    let q_s_a = node.w.load(Ordering::Relaxed) as f32 / (n as f32 + f32::EPSILON);
    let p_s_a = node.p.load(Ordering::Relaxed);
    let bias = f32::sqrt(parent_n as f32) / (1 + n) as f32;
    q_s_a + c * p_s_a * bias
}