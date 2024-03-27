#[macro_use]
extern crate rocket;
use dotenv::dotenv;

use rocket::serde::json::Json;

use rocket::State;
use rocket_okapi::settings::UrlObject;
use rocket_okapi::okapi::schemars::JsonSchema;

use rocket_okapi::swagger_ui::make_swagger_ui;
use rocket_okapi::{openapi, openapi_get_routes, rapidoc::*, swagger_ui::*};
use rusoto_autoscaling::Autoscaling;
use rusoto_core::Region;
use rusoto_ec2::{CreateLaunchTemplateRequest, Ec2, Ec2Client, RequestLaunchTemplateData};
use rusoto_elbv2::Elb;
use rusoto_route53::Route53;

use std::env;
use tokio::sync::Mutex;

use rocket::serde::{ Serialize, Deserialize};
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

#[derive(Serialize, Deserialize, JsonSchema)]
struct DeployAWSInput {
    instance_type: String,
    flake_url: String,
    files: Option<Vec<File>>,
    subdomain_prefix: String,
    min_size: Option<i64>,
    max_size: Option<i64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct DeployAWSOutput {
    id: String,
    launch_template_id: Option<String>,
}

impl DeployAWSOutput {
    fn new() -> Self {
        Self {
            id: "todo".to_string(),
            launch_template_id: None,
        }
    }
}


pub type OResult<T> = std::result::Result<rocket::serde::json::Json<T>, error::Error>;

/// Get instance ID from queue
///
/// Retrieves the next available EC2 instance ID from the queue.
#[openapi]
#[post("/deploy/aws/create", data = "<input>")]
async fn get_instance_id(state: &State<Mutex<AppState>>, input: Json<DeployAWSInput>) -> OResult<DeployAWSOutput>{
    let state = state.lock().await;
    let ec2_client = &state.ec2_client;
    let mut output = DeployAWSOutput::new();

    let launch_template_data = RequestLaunchTemplateData {
        image_id: Some("ami-abcdefgh".to_string()),
        instance_type: Some("t2.micro".to_string()),
        key_name: Some("my-key-pair".to_string()),
        // Add other parameters here as needed
        ..Default::default()
    };

    let create_launch_template_req = CreateLaunchTemplateRequest {
        launch_template_name: "MyLaunchTemplate".to_string(),
        version_description: Some("MyFirstVersion".to_string()),
        launch_template_data: launch_template_data,
        // Include any other needed fields
        ..Default::default()
    };

    let resp = ec2_client.create_launch_template(create_launch_template_req).await;
    match resp {
        Ok(res) => {
            println!("Launch template created: {:?}", res);
            output.launch_template_id = Some(res.launch_template.into_iter().next().unwrap().launch_template_id.unwrap());
            
        }
        Err(e) => {
           return Err(error::Error::new("LaunchTemplateCreationFailed", Some(&e.to_string()), 500));
        }
    }


    let as_client = &state.as_client;

    // create auto scaling group
    let create_asg_req = rusoto_autoscaling::CreateAutoScalingGroupType {
        auto_scaling_group_name: "todo".to_string(),
        launch_template: Some(rusoto_autoscaling::LaunchTemplateSpecification {
            launch_template_id: Some("todo".to_string()),
            version: Some("todo".to_string()),
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
            return Err(error::Error::new("AutoScalingGroupCreationFailed", Some(&e.to_string()), 500));
        }
    }

    let vpcs = ec2_client.describe_vpcs(
        // filter to default VPC
        rusoto_ec2::DescribeVpcsRequest {
            filters: Some(vec![rusoto_ec2::Filter {
                name: Some("isDefault".to_string()),
                values: Some(vec!["true".to_string()]),
            }]),
            ..Default::default()
        },
    ).await;

    let vpc_id = match vpcs {
        Ok(vpcs) => {
            vpcs.vpcs.unwrap().into_iter().next().unwrap().vpc_id.unwrap()
        }
        Err(e) => {
            return Err(error::Error::new("VPCFetchFailed", Some(&e.to_string()), 500));
        }
    };

    let subnets = ec2_client.describe_subnets(
        rusoto_ec2::DescribeSubnetsRequest {
            filters: Some(vec![rusoto_ec2::Filter {
                name: Some("vpc-id".to_string()),
                values: Some(vec![vpc_id.clone()]),
            }]),
            ..Default::default()
        },
    ).await;


    // create target group
    let elb_client = &state.elb_client;

    let create_target_group_req = rusoto_elbv2::CreateTargetGroupInput {
        name: "todo".to_string(),
        protocol: Some("HTTP".to_string()),
        port: Some(8000),
        vpc_id: Some(vpc_id.clone()),
        // Add other parameters here as needed
        ..Default::default()
    };

    let resp = elb_client.create_target_group(create_target_group_req).await;
    


    match resp {
        Ok(output) => {
            println!("Target group created: {:?}", output);
        }
        Err(e) => {
            return Err(error::Error::new("TargetGroupCreationFailed", Some(&e.to_string()), 500));
        }
    }

    // create load balancer
    let create_lb_req = rusoto_elbv2::CreateLoadBalancerInput {
        name: "todo".to_string(),
        subnets: Some(subnets.unwrap().subnets.unwrap().into_iter().map(|s| s.subnet_id.unwrap()).collect()),
        security_groups: None, // todo add security group
        // Add other parameters here as needed
        ..Default::default()
    };


    let resp = elb_client.create_load_balancer(create_lb_req).await;


    match resp {
        Ok(output) => {
            println!("Load balancer created: {:?}", output);
        }
        Err(e) => {
            return Err(error::Error::new("LoadBalancerCreationFailed", Some(&e.to_string()), 500));
        }
    }

    // todo wait for load balancer to be ready

    // create listener

    let create_listener_req = rusoto_elbv2::CreateListenerInput {
        default_actions: vec![rusoto_elbv2::Action {
            target_group_arn: Some("todo".to_string()),
            type_: "forward".to_string(),
            ..Default::default()

        }],
        load_balancer_arn: "todo".to_string(),
        port: Some(80),
        protocol: Some("HTTP".to_string()),
        // Add other parameters here as needed
        ..Default::default()
    };

    let resp = elb_client.create_listener(create_listener_req).await;

    match resp {
        Ok(output) => {
            println!("Listener created: {:?}", output);
        }
        Err(e) => {
            return Err(error::Error::new("ListenerCreationFailed", Some(&e.to_string()), 500));
        }
    }

    // attach target group to auto scaling group

    let attach_tg_req = rusoto_autoscaling::AttachLoadBalancerTargetGroupsType {
        auto_scaling_group_name: "todo".to_string(),
        target_group_ar_ns: vec!["todo".to_string()],
        // Add other parameters here as needed
        ..Default::default()
    };

    let resp = as_client.attach_load_balancer_target_groups(attach_tg_req).await;

    match resp {
        Ok(output) => {
            println!("Target group attached: {:?}", output);
        }
        Err(e) => {
            return Err(error::Error::new("TargetGroupAttachFailed", Some(&e.to_string()), 500));
        }
    }

    // create an A record in aws to match load balancer dns name to subdomain

    // create a route53 record set
    let record_set = rusoto_route53::ResourceRecordSet {
        name: "todo".to_string(),
        type_: "A".to_string(),
        alias_target: Some(rusoto_route53::AliasTarget {
            dns_name: "todo".to_string(),
            evaluate_target_health: true,
            hosted_zone_id: "todo".to_string(),
        }),
        // Add other parameters here as needed
        ..Default::default()
    };

    let change = rusoto_route53::Change {
        action: "CREATE".to_string(),
        resource_record_set: record_set,
    };

    let change_batch = rusoto_route53::ChangeBatch {
        changes:vec![change],
        comment: None,
    };

    let change_resource_record_sets_req = rusoto_route53::ChangeResourceRecordSetsRequest {
        change_batch: change_batch,
        hosted_zone_id: "todo".to_string(),
    };

    let route53_client = rusoto_route53::Route53Client::new(Region::default());

    let resp = route53_client.change_resource_record_sets(change_resource_record_sets_req).await;

    match resp {
        Ok(output) => {
            println!("Record set created: {:?}", output);
        }
        Err(e) => {
            return Err(error::Error::new("RecordSetCreationFailed", Some(&e.to_string()), 500));
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
