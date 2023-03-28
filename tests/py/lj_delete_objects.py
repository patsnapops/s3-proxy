import awswrangler as wr
wr.config.s3_endpoint_url='http://local.s3-proxy.patsnap.info'
import boto3
s = boto3.Session(profile_name='lj',region_name='us-east-1')
a='s3://datalake-internal.patsnap.com/data-financial-service/sznh_202303_sse_poc_company/'
wr.s3.delete_objects(path=a,boto3_session=s)