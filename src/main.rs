#[macro_use]
extern crate rocket;
use dotenv::dotenv;

use rocket::serde::json::Json;

use rocket::State;
use rocket_okapi::okapi::schemars::JsonSchema;
use rocket_okapi::settings::UrlObject;

use rocket_okapi::swagger_ui::make_swagger_ui;
use rocket_okapi::{openapi, openapi_get_routes, rapidoc::*, swagger_ui::*};
use rusoto_autoscaling::Autoscaling;
use rusoto_core::Region;
use rusoto_ec2::{CreateLaunchTemplateRequest, Ec2, Ec2Client, RequestLaunchTemplateData};
use rusoto_elbv2::{CreateTargetGroupError, CreateTargetGroupOutput, Elb};
use rusoto_route53::Route53;

use std::env;
use tokio::sync::Mutex;

use rocket::serde::{Deserialize, Serialize};
mod error;

struct AppState {
    ec2_client: Ec2Client,
    as_client: rusoto_autoscaling::AutoscalingClient,
    elb_client: rusoto_elbv2::ElbClient,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct File {
    content: String,
    path: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
struct Target {
    port: i64,
    health_check_path: Option<String>,
    health_check_enabled: Option<bool>,
}

struct Tarn {
    target: Target,
    arn: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct DeployAWSInput {
    instance_type: String,
    deployment_slug: String, // i am the deployment slug @_\/
    files: Option<Vec<File>>,
    subdomain_prefix: String,
    min_size: Option<i64>,
    max_size: Option<i64>,
    targets: Option<Vec<Target>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct DeployAWSOutput {
    id: String,
}

impl DeployAWSOutput {
    fn new() -> Self {
        Self { id: todo!() }
    }
}

pub type OResult<T> = std::result::Result<rocket::serde::json::Json<T>, error::Error>;

/// Get instance ID from queue
///
/// Retrieves the next available EC2 instance ID from the queue.
#[openapi]
#[post("/deploy/aws/create", data = "<input>")]
async fn get_instance_id(
    state: &State<Mutex<AppState>>,
    input: Json<DeployAWSInput>,
) -> OResult<DeployAWSOutput> {
    let state = state.lock().await;
    let ec2_client = &state.ec2_client;
    let mut output = DeployAWSOutput::new();

    let launch_template_data = RequestLaunchTemplateData {
        image_id: todo!(),
        instance_type: Some(input.instance_type.clone()),
        user_data: todo!(),
        // Add other parameters here as needed
        ..Default::default()
    };

    let create_launch_template_req = CreateLaunchTemplateRequest {
        launch_template_name: input.deployment_slug.clone(),
        launch_template_data: launch_template_data,
        // Include any other needed fields
        ..Default::default()
    };

    let resp = ec2_client
        .create_launch_template(create_launch_template_req)
        .await;
    match resp {
        Ok(res) => {
            println!("Launch template created: {:?}", res);
        }
        Err(e) => {
            return Err(error::Error::new(
                "LaunchTemplateCreationFailed",
                Some(&e.to_string()),
                500,
            ));
        }
    }

    let as_client = &state.as_client;

    // create auto scaling group
    let create_asg_req = rusoto_autoscaling::CreateAutoScalingGroupType {
        auto_scaling_group_name: input.deployment_slug.clone(),
        launch_template: Some(rusoto_autoscaling::LaunchTemplateSpecification {
            launch_template_name: Some(input.deployment_slug.clone()),
            ..Default::default()
        }),
        min_size: input.min_size.unwrap_or(1),
        max_size: input.max_size.unwrap_or(1),
        // desired_capacity: 1,
        // Add other parameters here as needed
        ..Default::default()
    };

    let resp = as_client.create_auto_scaling_group(create_asg_req).await;

    match resp {
        Ok(output) => {
            println!("Auto scaling group created: {:?}", output);
        }
        Err(e) => {
            return Err(error::Error::new(
                "AutoScalingGroupCreationFailed",
                Some(&e.to_string()),
                500,
            ));
        }
    }

    let vpc_id = env::var("VPC_ID").expect("VPC_ID must be set");
    let public_subnets: Vec<String> = env::var("PUBLIC_SUBNETS")
        .expect("PUBLIC_SUBNETS must be set")
        .split(",")
        .map(|s| s.to_string())
        .collect();

    // create target group
    let elb_client = &state.elb_client;

    let create_target_group_reqs = input
        .targets
        .unwrap_or(vec![Target {
            port: 8000,
            health_check_path: Some("/health".to_string()),
            health_check_enabled: Some(true),
        }])
        .iter()
        .map(|t| {
            rusoto_elbv2::CreateTargetGroupInput {
                name: input.deployment_slug.clone(),
                protocol: Some("HTTP".to_string()),
                port: Some(t.port),
                vpc_id: Some(vpc_id.clone()),
                health_check_path: t.health_check_path.clone(),
                health_check_enabled: t.health_check_enabled.clone(),
                // Add other parameters here as needed
                ..Default::default()
            }
        })
        .collect::<Vec<rusoto_elbv2::CreateTargetGroupInput>>();

    let mut target_group_arns = vec![];

    for req in create_target_group_reqs {
        let resp = elb_client.create_target_group(req).await;

        match resp {
            Ok(output) => {
                println!("Target group created: {:?}", output);
                if let Some(target_groups) = output.target_groups {
                    target_group_arns.push(target_groups[0].target_group_arn.clone());
                }
            }
            Err(e) => {
                return Err(error::Error::new(
                    "TargetGroupCreationFailed",
                    Some(&e.to_string()),
                    500,
                ));
            }
        }
    }

    let tarns = input
        .targets
        .unwrap_or(vec![Target {
            port: 8000,
            health_check_path: Some("/health".to_string()),
            health_check_enabled: Some(true),
        }])
        .iter()
        .zip(target_group_arns.iter())
        .map(|(t, arn)| Tarn {
            target: t.clone(),
            arn: arn.clone().unwrap(),
        })
        .collect::<Vec<Tarn>>();

    // create load balancer
    let create_lb_req = rusoto_elbv2::CreateLoadBalancerInput {
        name: input.deployment_slug.clone(),
        subnets: Some(public_subnets),
        security_groups: todo!(), // todo add security group
        // Add other parameters here as needed
        ..Default::default()
    };

    let resp = elb_client.create_load_balancer(create_lb_req).await;

    let lb_dns = match resp {
        Ok(output) => {
            println!("Load balancer created: {:?}", output);
            output.load_balancers.unwrap()[0].dns_name.clone()
        }
        Err(e) => {
            return Err(error::Error::new(
                "LoadBalancerCreationFailed",
                Some(&e.to_string()),
                500,
            ));
        }
    };
    let load_balancer_arn = match resp {
        Ok(output) => {
            println!("Load balancer created: {:?}", output);
            output.load_balancers.unwrap()[0].load_balancer_arn.clone()
        }
        Err(e) => {
            return Err(error::Error::new(
                "LoadBalancerCreationFailed",
                Some(&e.to_string()),
                500,
            ));
        }
    };

    // todo wait for load balancer to be ready

    // create listener

    let create_listener_reqs = tarns
        .iter()
        .map(|tarn| {
            rusoto_elbv2::CreateListenerInput {
                default_actions: vec![rusoto_elbv2::Action {
                    target_group_arn: Some(tarn.arn.clone()),
                    type_: "forward".to_string(),
                    ..Default::default()
                }],
                load_balancer_arn: load_balancer_arn.clone().unwrap(),
                port: Some(tarn.target.port),
                protocol: Some("HTTPS".to_string()),
                certificates: todo!(),
                ssl_policy: Some("ELBSecurityPolicy-2016-08".to_string()),
                // Add other parameters here as needed
                ..Default::default()
            }
        })
        .collect::<Vec<rusoto_elbv2::CreateListenerInput>>();

    for req in create_listener_reqs {
        let resp = elb_client.create_listener(req).await;

        match resp {
            Ok(output) => {
                println!("Listener created: {:?}", output);
            }
            Err(e) => {
                return Err(error::Error::new(
                    "ListenerCreationFailed",
                    Some(&e.to_string()),
                    500,
                ));
            }
        }
    }


    // attach target group to auto scaling group
    let attach_tg_req = rusoto_autoscaling::AttachLoadBalancerTargetGroupsType {
        auto_scaling_group_name: input.deployment_slug.clone(),
        target_group_ar_ns: target_group_arns
            .iter()
            .map(|t| t.clone().unwrap())
            .collect::<Vec<String>>(),
        // Add other parameters here as needed
        ..Default::default()
    };

    let resp = as_client
        .attach_load_balancer_target_groups(attach_tg_req)
        .await;

    match resp {
        Ok(output) => {
            println!("Target group attached: {:?}", output);
        }
        Err(e) => {
            return Err(error::Error::new(
                "TargetGroupAttachFailed",
                Some(&e.to_string()),
                500,
            ));
        }
    }

    // create an A record in aws to match load balancer dns name to subdomain

    // create a route53 record set
    let record_set = rusoto_route53::ResourceRecordSet {
        name: input.subdomain_prefix.clone(),
        type_: "A".to_string(),
        alias_target: Some(rusoto_route53::AliasTarget {
            dns_name: lb_dns.clone().unwrap(),
            evaluate_target_health: true,
            hosted_zone_id: todo!(),
        }),
        // Add other parameters here as needed
        ..Default::default()
    };

    let change = rusoto_route53::Change {
        action: "CREATE".to_string(),
        resource_record_set: record_set,
    };

    let change_batch = rusoto_route53::ChangeBatch {
        changes: vec![change],
        comment: None,
    };

    let change_resource_record_sets_req = rusoto_route53::ChangeResourceRecordSetsRequest {
        change_batch: change_batch,
        hosted_zone_id: todo!(),
    };

    let route53_client = rusoto_route53::Route53Client::new(Region::default());

    let resp = route53_client
        .change_resource_record_sets(change_resource_record_sets_req)
        .await;

    match resp {
        Ok(output) => {
            println!("Record set created: {:?}", output);
        }
        Err(e) => {
            return Err(error::Error::new(
                "RecordSetCreationFailed",
                Some(&e.to_string()),
                500,
            ));
        }
    }

    Ok(Json(output))
}

#[launch]
async fn rocket() -> _ {
    dotenv().ok();

    let ec2_client = Ec2Client::new(Region::default());
    let as_client = rusoto_autoscaling::AutoscalingClient::new(Region::default());
    let elb_client = rusoto_elbv2::ElbClient::new(Region::default());

    rocket::build()
        .configure(rocket::Config {
            address: "0.0.0.0".parse().expect("valid IP address"),
            port: 8000,
            ..rocket::Config::default()
        })
        .manage(Mutex::new(AppState {
            ec2_client,
            as_client,
            elb_client,
        }))
        .mount("/", openapi_get_routes![get_instance_id])
        .mount(
            "/swagger-ui/",
            make_swagger_ui(&SwaggerUIConfig {
                url: "../openapi.json".to_owned(),
                ..Default::default()
            }),
        )
        .mount(
            "/rapidoc/",
            make_rapidoc(&RapiDocConfig {
                general: GeneralConfig {
                    spec_urls: vec![UrlObject::new("General", "../openapi.json")],
                    ..Default::default()
                },
                hide_show: HideShowConfig {
                    allow_spec_url_load: false,
                    allow_spec_file_load: false,
                    ..Default::default()
                },
                ..Default::default()
            }),
        )
}
