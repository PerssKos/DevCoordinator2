//! Test-only deterministic deployment discovery fixture.

fn main() {
    let all_arguments = std::env::args().collect::<Vec<_>>();
    let protocol = if all_arguments
        .first()
        .is_some_and(|value| value.contains("protocol-v1"))
    {
        1
    } else if all_arguments
        .first()
        .is_some_and(|value| value.contains("protocol-v2"))
    {
        2
    } else {
        3
    };
    let arguments = all_arguments.into_iter().skip(1).collect::<Vec<_>>();
    if arguments.len() != 4
        || arguments[0] != "call"
        || arguments[1] != "deployment.list"
        || arguments[2] != "--params"
        || serde_json::from_str::<devcoordinator2_api::params::DeploymentList>(&arguments[3])
            .is_err()
    {
        eprintln!("fixture requires call deployment.list --params with typed JSON");
        std::process::exit(2);
    }
    let deployments = serde_json::json!({"deployments":[{
        "name":"fixture",
        "repository_name":"fixture",
        "route_port":9,
        "state":"running"
    }]});
    if protocol == 1 {
        println!(
            "{}",
            serde_json::json!({
                "protocol":1,"id":"formal-ui-self-test","ok":true,"result":deployments
            })
        );
    } else {
        println!(
            "{}",
            serde_json::json!({
                "protocol":protocol,
                "id":"formal-ui-self-test",
                "ok":true,
                "data":deployments
            })
        );
    }
}
