mkdir -p secrets
openssl genrsa -out secrets/dummy-sa.pem 2048
python3 -c '                                                                                                                                                            
  import json, sys                                                                                                                                                        
  key = open("secrets/dummy-sa.pem").read()                                                                                                                               
  json.dump({                                                                                                                                                             
    "type": "service_account",                 
    "project_id": "dummy",                                                                                                                                                
    "private_key_id": "dummy",                                                                                                                                            
    "private_key": key,                        
    "client_email": "dummy@dummy.iam.gserviceaccount.com",                                                                                                                
    "client_id": "0",                                             
    "token_uri": "https://oauth2.googleapis.com/token"                                                                                                                    
  }, sys.stdout)' >secrets/dummy-sa.json
echo "secrets/" >>.gitignore
