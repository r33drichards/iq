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
use rusoto_elbv2::Elb;
use rusoto_route53::Route53;

use std::env;
use tokio::sync::Mutex;

use rocket::serde::{Deserialize, Serialize};
mod error;

use uuid::Uuid;
// use base64::prelude::*;
use base64::{engine::general_purpose::URL_SAFE, Engine as _};

// let id = Uuid::new_v4();

struct AppState {
    ec2_client: Ec2Client,
    as_client: rusoto_autoscaling::AutoscalingClient,
    elb_client: rusoto_elbv2::ElbClient,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
struct File {
    content: String,
    path: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
struct Target {
    port: i64,
    health_check_path: Option<String>,
    health_check_enabled: Option<bool>,
}

struct Tarn {
    target: Target,
    arn: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
struct DeployAWSInput {
    flake_url: String,
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
    input: DeployAWSInput
}

impl DeployAWSOutput {
    fn new(input: DeployAWSInput ) -> Self {
        Self {
             id: Uuid::new_v4().to_string(),
             input: input,
            }
    }
}

pub type OResult<T> = std::result::Result<rocket::serde::json::Json<T>, error::Error>;


fn get_dir_path(fpath: &str) -> String {
    let mut parts = fpath.split('/').collect::<Vec<&str>>();
    parts.pop();
    parts.join("/").to_string()
}
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
    println!("Input: {:?}", input.0.clone().deployment_slug);
    let output = DeployAWSOutput::new(input.0.clone());

    let launch_template_data = RequestLaunchTemplateData {
        image_id: Some("ami-0e94c086e49480566".to_string()),
        
        instance_type: Some(input.instance_type.clone()),
        block_device_mappings: Some(vec![rusoto_ec2::LaunchTemplateBlockDeviceMappingRequest {
            device_name: Some("/dev/xvda".to_string()),
            ebs: Some(rusoto_ec2::LaunchTemplateEbsBlockDeviceRequest {
                volume_size: Some(100),
                volume_type: Some("gp3".to_string()),
                delete_on_termination: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        user_data: {
            let mut user_data = "#!/bin/bash\n".to_string();
            input.files.as_ref().unwrap_or(
                &vec![]
            ).clone().iter().for_each(|f| {
               let dirpath = get_dir_path(&f.path);
     
                user_data
                    .push_str(format!(
                        "mkdir -p {} && echo {} | base64 -d > {}\n",
                        dirpath,
                        URL_SAFE.encode(f.content.clone()),
                        f.path,
                    ).as_str())
            });
            // err = session.Run(fmt.Sprintf("nixos-rebuild switch --impure --flake '%s'", flake_url))
            user_data.push_str(format!(
                "nixos-rebuild switch --impure --flake '{}'\n",
                input.flake_url
            ).as_str());


            Some(URL_SAFE.encode(user_data).to_string())
        },
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
        vpc_zone_identifier: Some("subnet-07789005966d047bf".to_string()),
        // availability_zones: Some(vec!["us-west-1a".to_string(), "us-west-1c".to_string()]),
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

    let vpc_id = "vpc-031c620b47a9ea885".to_string();
    let public_subnets = vec!["subnet-040ebc679c54ecf38".to_string(), "subnet-0e22657a6f50a3235".to_string()];
    // create target group
    let elb_client = &state.elb_client;

    let create_target_group_reqs = input
        .targets.as_ref()
        .unwrap_or(&vec![Target {
            port: 8000,
            ..Default::default()
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
        .targets.as_ref()
        .unwrap_or(&vec![Target {
            port: 8000,
            ..Default::default()
        }])
        .iter()
        .zip(target_group_arns.iter())
        .map(|(t, arn)| Tarn {
            target: t.clone(),
            arn: arn.clone().unwrap(),
        })
        .collect::<Vec<Tarn>>();

    // create security groups
    let create_sg_req = rusoto_ec2::CreateSecurityGroupRequest {
        description: "Security group for the deployment".to_string(),
        group_name: input.deployment_slug.clone(),
        vpc_id: Some(vpc_id.clone()),
        // Add other parameters here as needed
        ..Default::default()
    };

    let resp = ec2_client.create_security_group(create_sg_req).await;
    let sg_id = match resp {
        Ok(output) => {
            println!("Security group created: {:?}", output);
            output.group_id.clone()
        }
        Err(e) => {
            return Err(error::Error::new(
                "SecurityGroupCreationFailed",
                Some(&e.to_string()),
                500,
            ));
        }
    };
    // rusoto_ec2::AuthorizeSecurityGroupIngressRequest {
    //     group_id:sg_id.clone(),
    //     // Add other parameters here as needed
    //     ..Default::default()
    // };

    let authorize_sg_reqs = input.targets.as_ref().unwrap_or(&vec![Target {
        port: 8000,
        ..Default::default()
    }]).iter().map(|t| {
        rusoto_ec2::AuthorizeSecurityGroupIngressRequest {
            group_id: sg_id.clone(),
            from_port: Some(t.port),
            to_port: Some(t.port),
            ip_protocol: Some("TCP".to_string()),
            cidr_ip: Some("0.0.0.0/0".to_string()),
            ..Default::default()
        }}).collect::<Vec<rusoto_ec2::AuthorizeSecurityGroupIngressRequest>>(); 

    for req in authorize_sg_reqs {
        let resp = ec2_client.authorize_security_group_ingress(req).await;

        match resp {
            Ok(output) => {
                println!("Security group ingress rules added: {:?}", output);
            }
            Err(e) => {
                return Err(error::Error::new(
                    "SecurityGroupIngressRulesAdditionFailed",
                    Some(&e.to_string()),
                    500,
                ));
            }
        }
    }





    // create load balancer
    let create_lb_req = rusoto_elbv2::CreateLoadBalancerInput {
        name: input.deployment_slug.clone(),
        subnets: Some(public_subnets),
        security_groups: Some(vec![sg_id.expect("sg should be set")]), // todo add security group
        // Add other parameters here as needed
        ..Default::default()
    };

    let resp = elb_client.create_load_balancer(create_lb_req).await;

    let  (lb_dns,load_balancer_arn) = 
    match resp {
        Ok(output) => {
            println!("Load balancer created: {:?}", output);
            let lb_dns = output.load_balancers.as_ref().unwrap()[0].dns_name.clone();
            let load_balancer_arn = output.load_balancers.as_ref().unwrap()[0].load_balancer_arn.clone();
            (lb_dns,load_balancer_arn)
        }
        Err(e) => {
            return Err(error::Error::new(
                "LoadBalancerCreationFailed",
                Some(&e.to_string()),
                500,
            ));
        }
    };


    // wait for load balancer to be ready

    for _ in 0..100 {
        let resp = elb_client
            .describe_load_balancers(rusoto_elbv2::DescribeLoadBalancersInput {
                load_balancer_arns: Some(vec![load_balancer_arn.clone().unwrap()]),
                ..Default::default()
            })
            .await;

        match resp {
            Ok(output) => {
                let state = output.load_balancers.as_ref().unwrap()[0].state.as_ref().unwrap();
                if let Some(code) = state.code.as_ref() {
                    if code == "active" {
                        break;
                    }
                }
            }
            Err(e) => {
                return Err(error::Error::new(
                    "LoadBalancerStateCheckFailed",
                    Some(&e.to_string()),
                    500,
                ));
            }
        }
        // sleep for 3 seconds
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    // pub struct Certificate {
    //     /// <p>The Amazon Resource Name (ARN) of the certificate.</p>
    //     pub certificate_arn: Option<String>,
    //     /// <p>Indicates whether the certificate is the default certificate. Do not set this value when specifying a certificate as an input. This value is not included in the output when describing a listener, but is included when describing listener certificates.</p>
    //     pub is_default: Option<bool>,
    // }


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
                protocol: Some("HTTP".to_string()),
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
    // {"err":"RecordSetCreationFailed","msg":"Request ID: Some(\"c203136a-5083-4d3b-8b3c-989c978cd68a\") Body: <?xml version=\"1.0\"?>\n<ErrorResponse xmlns=\"https://route53.amazonaws.com/doc/2013-04-01/\"><Error><Type>Sender</Type><Code>SignatureDoesNotMatch</Code><Message>Credential should be scoped to a valid region. </Message></Error><RequestId>c203136a-5083-4d3b-8b3c-989c978cd68a</RequestId></ErrorResponse>"}%                                                                                                                                        
    let record_set = rusoto_route53::ResourceRecordSet {
        name: input.subdomain_prefix.clone(),
        type_: "CNAME".to_string(),
        ttl: Some(300),
        region: Some("us-west-1".to_string()), // todo get region from ec2 client
        resource_records: Some(vec![rusoto_route53::ResourceRecord {
            value: lb_dns.clone().unwrap(),
        }]),

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
        hosted_zone_id: "Z03309493AGZOVY2IU47X".to_string(),
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

#[rocket::main]
async fn main() {
    dotenv().ok();

    let ec2_client = Ec2Client::new(Region::default());
    let as_client = rusoto_autoscaling::AutoscalingClient::new(Region::default());
    let elb_client = rusoto_elbv2::ElbClient::new(Region::default());

    let _ = rocket::build()
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
            })).launch().await;
}

