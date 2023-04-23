use crate::{
    environment::{Environment, GameStatus, Turn},
    model::Model,
};
use rand::{distributions::Uniform, thread_rng, Rng};
use std::{collections::VecDeque, fmt::Display};
use tensorflow::{
    ops::{assign, constant, mean, reshape, square, sub, GatherNd, Placeholder},
    train::{AdadeltaOptimizer, MinimizeOptions, Optimizer},
    DataType, Operation, Session, SessionOptions, SessionRunArgs, Status, Tensor, Variable,
};

pub struct Transition {
    pub state: [f32; Environment::BOARD_SIZE * Environment::BOARD_SIZE],
    pub next_state: Option<[f32; Environment::BOARD_SIZE * Environment::BOARD_SIZE]>,
    pub action: usize,
    pub reward: f32,
}

impl Display for Transition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for row in 0..Environment::BOARD_SIZE {
            for col in 0..Environment::BOARD_SIZE {
                let index = row * Environment::BOARD_SIZE + col;
                let stone = self.state[index];
                write!(
                    f,
                    "{}",
                    if index == self.action {
                        "A"
                    } else if f32::abs(stone) < f32::EPSILON {
                        "-"
                    } else if 0f32 < stone {
                        "O"
                    } else {
                        "X"
                    }
                )?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

pub struct TrainSession {
    pub model: Model,
    pub session: Session,
    pub optimizer: AdadeltaOptimizer,
    pub optimizer_vars: Vec<Variable>,
    pub op_minimize: Operation,
    pub copy_ops: Vec<Operation>,
    pub op_input: Operation,
    pub op_output: Operation,
    pub op_target_input: Operation,
    pub op_target_output: Operation,
    pub op_input_target_q: Operation,
    pub op_input_action: Operation,
    pub op_loss: Operation,
    pub replay_memory: VecDeque<Transition>,
    pub played_turn_count: u128,
}

impl TrainSession {
    pub fn new(mut model: Model) -> Result<Self, Status> {
        let mut copy_ops = Vec::new();

        for index in 0..model.variables.len() {
            copy_ops.push(assign(
                model.target_variables[index].output().clone(),
                model.variables[index].output().clone(),
                &mut model.scope,
            )?);
        }

        let op_input_target_q = Placeholder::new()
            .dtype(DataType::Float)
            .shape([-1i64, 1])
            .build(&mut model.scope.with_op_name("input_target_q"))?;

        // Shape: [batch_size, 2]
        // First column: batch index
        let op_input_action = Placeholder::new()
            .dtype(DataType::Int32)
            .shape([-1i64, 2])
            .build(&mut model.scope.with_op_name("input_action"))?;
        let output = model.scope.graph().operation_by_name_required("output")?;
        let flatten_output = reshape(
            output,
            constant(
                &[
                    -1i64,
                    Environment::BOARD_SIZE as i64 * Environment::BOARD_SIZE as i64,
                ],
                &mut model.scope,
            )?,
            &mut model.scope,
        )?;
        let q = GatherNd::new()
            .Tparams(DataType::Float)
            .Tindices(DataType::Int32)
            .build(flatten_output, op_input_action.clone(), &mut model.scope)?;

        let op_loss = mean(
            square(
            sub(op_input_target_q.clone(), q, &mut model.scope)?,
            &mut model.scope,
            )?,
            constant(&[0], &mut model.scope)?,
            &mut model.scope,
        )?;

        let optimizer = AdadeltaOptimizer::new();
        let (optimizer_vars, op_minimize) = optimizer.minimize(
            &mut model.scope,
            op_loss.output(0),
            MinimizeOptions::default().with_variables(&model.variables),
        )?;

        let session = Session::new(&SessionOptions::new(), &model.scope.graph())?;
        let op_input = model.scope.graph().operation_by_name_required("input")?;
        let op_output = model.scope.graph().operation_by_name_required("output")?;
        let op_target_input = model
            .scope
            .graph()
            .operation_by_name_required("target_input")?;
        let op_target_output = model
            .scope
            .graph()
            .operation_by_name_required("target_output")?;

        Ok(Self {
            model,
            session,
            optimizer,
            optimizer_vars,
            op_minimize,
            copy_ops,
            op_input,
            op_output,
            op_target_input,
            op_target_output,
            op_input_target_q,
            op_input_action,
            op_loss,
            replay_memory: VecDeque::with_capacity(100_0000),
            played_turn_count: 0,
        })
    }

    pub fn init(&self) -> Result<(), Status> {
        let mut init = SessionRunArgs::new();

        for variable in &self.model.variables {
            init.add_target(variable.initializer());
        }

        for variable in &self.optimizer_vars {
            init.add_target(variable.initializer());
        }

        self.session.run(&mut init)?;

        let mut copy = SessionRunArgs::new();

        for op in &self.copy_ops {
            copy.add_target(op);
        }

        self.session.run(&mut copy)?;
        Ok(())
    }

    pub fn warm_up(&mut self) {
        let mut rng = thread_rng();
        let mut draw = 0;
        let mut black_win = 0;
        let mut white_win = 0;

        loop {
            let mut env = Environment::new();
            let mut turn = env.turn;

            loop {
                let mut board = [0f32; Environment::BOARD_SIZE * Environment::BOARD_SIZE];
                env.copy_board(turn, &mut board);

                let legal_moves = env
                    .legal_moves
                    .iter()
                    .enumerate()
                    .filter_map(|(index, is_legal)| if *is_legal { Some(index) } else { None })
                    .collect::<Vec<_>>();
                let random_move = legal_moves[rng.gen_range(0..legal_moves.len())];

                let (reward, next_board) = match env.place_stone(random_move) {
                    GameStatus::InProgress => {
                        let mut next_board =
                            [0f32; Environment::BOARD_SIZE * Environment::BOARD_SIZE];
                        env.copy_board(turn, &mut next_board);
                        (0f32, Some(next_board))
                    }
                    GameStatus::Draw => {
                        draw += 1;
                        (0f32, None)
                    }
                    GameStatus::BlackWin => {
                        black_win += 1;
                        (if turn == Turn::Black { 1f32 } else { -1f32 }, None)
                    }
                    GameStatus::WhiteWin => {
                        white_win += 1;
                        (if turn == Turn::White { 1f32 } else { -1f32 }, None)
                    }
                };

                let has_next_board = next_board.is_some();

                self.replay_memory.push_back(Transition {
                    state: board,
                    next_state: next_board,
                    action: random_move,
                    reward,
                });
                turn = env.turn;

                if self.replay_memory.len() == 50_000 {
                    println!(
                        "Draw: {}, Black Win: {}, White Win: {}",
                        draw, black_win, white_win
                    );
                    return;
                }

                if !has_next_board {
                    break;
                }
            }
        }
    }

    pub fn perform_episodes(&mut self, count: usize) -> Result<(), Status> {
        let mut rng = thread_rng();

        for _ in 0..count {
            let mut env = Environment::new();
            let mut turn = env.turn;

            loop {
                let mut board = [0f32; Environment::BOARD_SIZE * Environment::BOARD_SIZE];
                env.copy_board(turn, &mut board);

                let epsilon =
                    f64::max(0.1f64, 1f64 - (self.played_turn_count as f64 / 100_0000f64));
                let selected_move = if rng.gen_bool(epsilon) {
                    let legal_moves = env
                        .legal_moves
                        .iter()
                        .enumerate()
                        .filter_map(|(index, is_legal)| if *is_legal { Some(index) } else { None })
                        .collect::<Vec<_>>();
                    legal_moves[rng.gen_range(0..legal_moves.len())]
                } else {
                    let mut tensor = Tensor::new(&[
                        1,
                        Environment::BOARD_SIZE as u64,
                        Environment::BOARD_SIZE as u64,
                        1,
                    ]);
                    tensor[..].copy_from_slice(&board);

                    let mut eval_run_args = SessionRunArgs::new();
                    eval_run_args.add_feed(&self.op_input, 0, &tensor);
                    eval_run_args.add_target(&self.op_output);

                    let fetch_token = eval_run_args.request_fetch(&self.op_output, 0);
                    self.session.run(&mut eval_run_args)?;

                    let output = eval_run_args.fetch::<f32>(fetch_token)?;
                    let (index, _) = output[..]
                        .iter()
                        .enumerate()
                        .max_by(|(_, q_lhs), (_, q_rhs)| f32::total_cmp(&q_lhs, &q_rhs))
                        .unwrap();

                    index
                };

                let (reward, next_board) = match env.place_stone(selected_move) {
                    GameStatus::InProgress => {
                        let mut next_board =
                            [0f32; Environment::BOARD_SIZE * Environment::BOARD_SIZE];
                        env.copy_board(turn, &mut next_board);
                        (0f32, Some(next_board))
                    }
                    GameStatus::Draw => (0f32, None),
                    GameStatus::BlackWin => (if turn == Turn::Black { 1f32 } else { -1f32 }, None),
                    GameStatus::WhiteWin => (if turn == Turn::White { 1f32 } else { -1f32 }, None),
                };

                let has_next_board = next_board.is_some();

                if self.replay_memory.len() == 100_0000 {
                    self.replay_memory.pop_front();
                }

                self.replay_memory.push_back(Transition {
                    state: board,
                    next_state: next_board,
                    action: selected_move,
                    reward,
                });
                turn = env.turn;

                let mut tensor_input = Tensor::<f32>::new(&[
                    32,
                    Environment::BOARD_SIZE as u64,
                    Environment::BOARD_SIZE as u64,
                    1,
                ]);
                let mut tensor_target_q = Tensor::<f32>::new(&[32]);
                let mut tensor_action = Tensor::<i32>::new(&[32, 2]);

                let dist = Uniform::new(0, self.replay_memory.len());
                let mut sampler = thread_rng().sample_iter(dist);

                for batch_index in 0usize..32 {
                    let transition = &self.replay_memory[sampler.next().unwrap()];
                    let target_q = match &transition.next_state {
                        Some(next_state) => {
                            let mut tensor = Tensor::new(&[
                                1,
                                Environment::BOARD_SIZE as u64,
                                Environment::BOARD_SIZE as u64,
                                1,
                            ]);
                            tensor[..].copy_from_slice(next_state);

                            let mut eval_run_args = SessionRunArgs::new();
                            eval_run_args.add_feed(&self.op_input, 0, &tensor);
                            eval_run_args.add_target(&self.op_output);

                            let fetch_token = eval_run_args.request_fetch(&self.op_output, 0);
                            self.session.run(&mut eval_run_args)?;

                            let output = eval_run_args.fetch::<f32>(fetch_token)?;
                            let future_q = *output[..]
                                .iter()
                                .max_by(|q_lhs, q_rhs| f32::total_cmp(q_lhs, q_rhs))
                                .unwrap();
                            transition.reward + 0.95 * future_q
                        }
                        None => transition.reward,
                    };
                    tensor_input[batch_index * Environment::BOARD_SIZE * Environment::BOARD_SIZE
                        ..(batch_index + 1) * Environment::BOARD_SIZE * Environment::BOARD_SIZE]
                        .copy_from_slice(&transition.state);
                    tensor_target_q[batch_index] = target_q;
                    tensor_action[batch_index * 2] = batch_index as i32;
                    tensor_action[batch_index * 2 + 1] = transition.action as i32;
                }

                let mut train_run_args = SessionRunArgs::new();
                train_run_args.add_feed(&self.op_input, 0, &tensor_input);
                train_run_args.add_feed(&self.op_input_target_q, 0, &tensor_target_q);
                train_run_args.add_feed(&self.op_input_action, 0, &tensor_action);
                train_run_args.add_target(&self.op_minimize);
                self.session.run(&mut train_run_args)?;
                self.played_turn_count += 1;

                if self.played_turn_count % 1_0000 == 0 {
                    println!(
                        "Played {} turns, updating target network.",
                        self.played_turn_count
                    );

                    let mut loss_run_args = SessionRunArgs::new();
                    loss_run_args.add_feed(&self.op_input, 0, &tensor_input);
                    loss_run_args.add_feed(&self.op_input_target_q, 0, &tensor_target_q);
                    loss_run_args.add_feed(&self.op_input_action, 0, &tensor_action);

                    let loss_token = loss_run_args.request_fetch(&self.op_loss, 0);
                    loss_run_args.add_target(&self.op_loss);
                    self.session.run(&mut loss_run_args)?;

                    let loss = loss_run_args.fetch::<f32>(loss_token)?;
                    println!("Loss: {}", loss);

                    let mut update_run_args = SessionRunArgs::new();

                    for op in &self.copy_ops {
                        update_run_args.add_target(op);
                    }

                    self.session.run(&mut update_run_args)?;
                }

                if !has_next_board {
                    break;
                }
            }
        }

        Ok(())
    }
}
