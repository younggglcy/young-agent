use young_agent_runtime::RunStopToken;

use crate::args::SignalAction;

pub(crate) fn install_signal_handler(
    action: SignalAction,
    stop: RunStopToken,
) -> Result<(), ctrlc::Error> {
    ctrlc::try_set_handler(move || match action {
        SignalAction::Interrupt => stop.interrupt("process signal requested interruption"),
        SignalAction::Cancel => stop.cancel("process signal requested cancellation"),
    })
}
